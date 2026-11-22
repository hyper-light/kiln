//! Region-local groups under a global root (CP-T1.11, G14 step 1; docs/design/regions.md).
//!
//! - AC-T1.11.4: the corpus at one node, three nodes and three regions (simulated) gives the same
//!   answers and rows.
//! - Placement rules hold under member loss: a regional group's voters and lead stay in its home
//!   region (refused, never borrowed from another region, when too few are left), and the root
//!   stays quorate with a region lost.
//! - AC-T1.11.2's lab, one tiny instance: regional writes commit at the within-region latency it
//!   is given, global ones pay the between-region one. The 60 ms lab and its p99 are the
//!   coordinator's (examples/regions_lab.rs).

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

use std::collections::BTreeMap;
use std::time::Duration;

use amphora::regions::lab::{Lab, Latency, Op, Shape, quantile};
use amphora::regions::{
    Atlas, Members, Placement, Refused, Region, Rule, Scope, check, place, quorate,
};
use amphora::reshard::NodeId;

fn r(s: &str) -> Region {
    Region(s.to_owned())
}

fn homes() -> Vec<Region> {
    vec![r("a"), r("b"), r("c")]
}

fn one_node() -> Shape {
    Shape {
        members: vec![(r("a"), 1)],
        homes: homes(),
        groups: 4,
        survive: 0,
        survive_regions: 0,
    }
}

fn three_nodes() -> Shape {
    Shape {
        members: vec![(r("a"), 3)],
        homes: homes(),
        groups: 4,
        survive: 1,
        survive_regions: 0,
    }
}

fn three_regions() -> Shape {
    Shape {
        members: vec![(r("a"), 3), (r("b"), 3), (r("c"), 3)],
        homes: homes(),
        groups: 4,
        survive: 1,
        survive_regions: 1,
    }
}

/// The test's latencies: inputs to the lab, not Amphora's.
fn latency() -> Latency {
    Latency {
        within: Duration::from_micros(500),
        between: Duration::from_millis(60),
    }
}

/// A corpus of puts and gets by six clients over a small key space (so clients in different
/// regions write the same keys), regional and global, from a seed: SplitMix64.
fn corpus(seed: u64, n: usize) -> Vec<Op> {
    let mut s = seed;
    let mut next = move || {
        s = s.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = s;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    };
    (0..n)
        .map(|_| {
            let x = next();
            let client = usize::try_from(x % 6).unwrap();
            let key = format!("k{}", (x >> 8) % 40);
            let global = (x >> 16).is_multiple_of(5);
            if (x >> 24).is_multiple_of(3) {
                Op::Get {
                    client,
                    key,
                    global,
                }
            } else {
                Op::Put {
                    client,
                    key,
                    value: x >> 32,
                    global,
                }
            }
        })
        .collect()
}

#[test]
fn ac_t1_11_4_corpus_identical_at_one_node_three_nodes_three_regions() {
    for seed in 0..20 {
        let ops = corpus(seed, 400);
        let outcomes: Vec<_> = [one_node(), three_nodes(), three_regions()]
            .iter()
            .map(|shape| Lab::new(shape, latency()).unwrap().run(&ops).unwrap())
            .collect();
        // Non-vacuity: the corpus read something back, and wrote rows in every scope.
        assert!(outcomes[0].answers.iter().any(Option::is_some));
        assert!(outcomes[0].answers.iter().any(Option::is_none));
        for scope in [Scope::Global, Scope::Region(r("a")), Scope::Region(r("b"))] {
            assert!(outcomes[0].rows.keys().any(|(s, _)| *s == scope));
        }
        for o in &outcomes[1..] {
            assert_eq!(o.answers, outcomes[0].answers, "seed {seed}");
            assert_eq!(o.rows, outcomes[0].rows, "seed {seed}");
        }
    }
}

#[test]
fn routing_reads_try_the_readers_region_first() {
    let mut regions = BTreeMap::new();
    for h in homes() {
        regions.insert(h, amphora::groups::even(NodeId(0), 4).unwrap());
    }
    let atlas = Atlas::new(amphora::groups::even(NodeId(0), 4).unwrap(), regions);
    for from in homes() {
        let order = atlas.read(&from, "some key").unwrap();
        assert_eq!(order.len(), 3);
        assert_eq!(order[0].scope, Scope::Region(from.clone()));
        let rest: Vec<_> = order[1..].iter().map(|a| a.scope.clone()).collect();
        let expect: Vec<_> = homes()
            .into_iter()
            .filter(|h| *h != from)
            .map(Scope::Region)
            .collect();
        assert_eq!(rest, expect);
    }
    assert_eq!(atlas.read(&r("z"), "k"), Err(Refused::NoRegion(r("z"))));
}

#[test]
fn a_regional_group_is_led_and_quorate_in_its_region_under_member_loss() {
    let mut members = Members::new();
    for n in 0..4 {
        members.join(NodeId(n), r("a"));
    }
    for n in 4..8 {
        members.join(NodeId(n), r("b"));
    }
    let rule = Rule::Home {
        region: r("a"),
        survive: 1,
    };
    let first = place(&members, &rule, None).unwrap();
    assert_eq!(first.voters, vec![NodeId(0), NodeId(1), NodeId(2)]);
    check(&members, &rule, &first).unwrap();

    // The lead is lost: it is replaced from its region, the other voters stay.
    members.lose(first.lead);
    let second = place(&members, &rule, Some(&first)).unwrap();
    check(&members, &rule, &second).unwrap();
    assert_eq!(second.voters, vec![NodeId(1), NodeId(2), NodeId(3)]);
    assert!(quorate(&members, &second));
    for v in &second.voters {
        assert_eq!(members.region_of(*v), Some(&r("a")));
    }

    // Too few left in the region: refused, never a voter from region b.
    members.lose(NodeId(1));
    assert_eq!(
        place(&members, &rule, Some(&second)),
        Err(Refused::TooFewMembers {
            region: r("a"),
            live: 2,
            needs: 3
        })
    );
    // The placement it keeps is still in its region and still has a quorum.
    assert!(quorate(&members, &second));

    // A voter outside the home region, or a lead that isn't a voter, breaks the rule.
    let outside = Placement {
        voters: vec![NodeId(2), NodeId(3), NodeId(4)],
        lead: NodeId(2),
    };
    assert!(check(&members, &rule, &outside).is_err());
    let stray = Placement {
        voters: vec![NodeId(0), NodeId(2), NodeId(3)],
        lead: NodeId(5),
    };
    assert!(check(&members, &rule, &stray).is_err());
}

#[test]
fn the_root_spans_regions_and_survives_losing_one() {
    let mut members = Members::new();
    let mut n = 0;
    for region in ["a", "b", "c"] {
        for _ in 0..3 {
            members.join(NodeId(n), r(region));
            n += 1;
        }
    }
    let rule = Rule::Global {
        survive_regions: 1,
        survive: 0,
        lead: Some(r("a")),
    };
    let root = place(&members, &rule, None).unwrap();
    check(&members, &rule, &root).unwrap();
    assert_eq!(root.voters, vec![NodeId(0), NodeId(3), NodeId(6)]);
    assert_eq!(members.region_of(root.lead), Some(&r("a")));

    // Region b lost: still quorate as placed.
    members.lose_region(&r("b"));
    assert!(quorate(&members, &root));
    // Placed again, it can't keep a voter in each of three regions: refused, typed.
    assert_eq!(
        place(&members, &rule, Some(&root)),
        Err(Refused::TooFewRegions { have: 2, needs: 3 })
    );
    // A voter of the root lost inside a region: replaced from the same region.
    members.restore(NodeId(3));
    members.restore(NodeId(4));
    members.restore(NodeId(5));
    members.lose(NodeId(3));
    let again = place(&members, &rule, Some(&root)).unwrap();
    check(&members, &rule, &again).unwrap();
    assert_eq!(again.voters, vec![NodeId(0), NodeId(4), NodeId(6)]);

    // Two of three voters in one region: losing it would leave no quorum.
    let lopsided = Placement {
        voters: vec![NodeId(0), NodeId(1), NodeId(6)],
        lead: NodeId(0),
    };
    assert!(check(&members, &rule, &lopsided).is_err());
}

#[test]
fn lab_placements_hold_under_member_and_region_loss() {
    let mut lab = Lab::new(&three_regions(), latency()).unwrap();
    // Three members a region, so losing one leaves two, fewer than the three voters both region
    // b's groups and the root's share of b need: refused, nothing placed outside the rules.
    let before: Vec<_> = lab.placements().map(|(_, _, p)| p.clone()).collect();
    assert!(matches!(
        lab.lose(NodeId(4)),
        Err(Refused::TooFewMembers { .. } | Refused::TooFewRegions { .. })
    ));
    let after: Vec<_> = lab.placements().map(|(_, _, p)| p.clone()).collect();
    assert_eq!(before, after);

    let shape = Shape {
        members: vec![(r("a"), 4), (r("b"), 4), (r("c"), 4)],
        ..three_regions()
    };
    let mut lab = Lab::new(&shape, latency()).unwrap();
    for lost in [NodeId(0), NodeId(5), NodeId(10)] {
        lab.lose(lost).unwrap();
        for (_, rule, placement) in lab.placements() {
            check(lab.members(), rule, placement).unwrap();
            assert!(!placement.voters.contains(&lost));
            assert!(quorate(lab.members(), placement));
        }
    }

    // Region c lost: the root and the other regions' groups still commit; c's groups don't.
    let shape = Shape {
        members: vec![(r("a"), 3), (r("b"), 3), (r("c"), 3)],
        survive: 0,
        ..three_regions()
    };
    let mut lab = Lab::new(&shape, latency()).unwrap();
    let before = lab.run(&corpus(7, 50)).unwrap();
    lab.lose_region(&r("c"));
    let mut ok = 0;
    for (address, _, placement) in lab.placements() {
        let live = quorate(lab.members(), placement);
        assert_eq!(live, address.scope != Scope::Region(r("c")), "{address:?}");
        ok += usize::from(live);
    }
    assert!(ok > 0);
    let write_a = Op::Put {
        client: 0,
        key: "after".into(),
        value: 1,
        global: false,
    };
    let write_g = Op::Put {
        client: 1,
        key: "after".into(),
        value: 2,
        global: true,
    };
    let after = lab.run(&[write_a, write_g]).unwrap();
    assert_eq!(after.regional_commits.len(), 1);
    assert_eq!(after.global_commits.len(), 1);
    assert!(after.rows.len() > before.rows.len());
    let write_c = Op::Put {
        client: 2,
        key: "after".into(),
        value: 3,
        global: false,
    };
    assert!(lab.run(&[write_c]).is_err());
}

#[test]
fn ac_t1_11_2_lab_tiny_regional_commits_stay_in_region() {
    let mut lab = Lab::new(&three_regions(), latency()).unwrap();
    let out = lab.run(&corpus(3, 300)).unwrap();
    assert!(!out.regional_commits.is_empty() && !out.global_commits.is_empty());
    let l = latency();
    // A regional write: the client's round trip to its lead and the lead's to its region's
    // quorum, both within the region.
    for c in &out.regional_commits {
        assert_eq!(*c, l.within * 2);
    }
    // A global write: the root's quorum spans regions.
    for c in &out.global_commits {
        assert!(*c >= l.between);
    }
    let p99 = quantile(&out.regional_commits, 990).unwrap();
    assert!(p99 < l.between);
    assert_eq!(quantile(&[], 990), None);
    let samples: Vec<Duration> = (1..=100).map(Duration::from_millis).collect();
    assert_eq!(quantile(&samples, 990), Some(Duration::from_millis(99)));
    assert_eq!(quantile(&samples, 500), Some(Duration::from_millis(50)));
}
