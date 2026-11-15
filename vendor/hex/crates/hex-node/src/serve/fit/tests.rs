//! The node's descriptors fitted on macOS and Windows (ROOT's one accounting of 2026-11-13): within the platform's
//! limit with or without hosted VMs, the most places that fit, the hosted VMs' lanes among the terms, and a second
//! hosted VM splitting them.

use super::*;

fn platform(descriptors: usize, programs: usize, threads: usize) -> Platform {
    Platform {
        descriptors,
        threads,
        programs,
    }
}

/// Platforms as macOS and Windows nodes read them, and tighter ones: macOS's soft limit raised to
/// `kern.maxfilesperproc`'s 245,760 with a user's process limit, its default soft limit of 10,240, Windows' 2^24 handles
/// with half of them as programs, and a limit too tight for the platform's places.
fn platforms() -> [Platform; 4] {
    [
        platform(245_760, 2_666, 8_192),
        platform(10_240, 2_666, 8_192),
        platform(1 << 24, 1 << 23, 1_000_000),
        platform(512, 100_000, 64),
    ]
}

/// The terms' sum at a fitted node's bounds is within its platform's descriptors, with no hosted VM and with one,
/// beside what it held at its start; one more place would pass it (or the places are the platform's own). Do: fit each
/// platform, holding 37 descriptors beside. Expect: within the limit, and the most places that are.
#[test]
fn a_fitted_nodes_terms_are_within_its_descriptors_at_the_most_places_that_fit() {
    for own in [MACOS, WINDOWS] {
        for p in platforms() {
            for hosted in [0, 1] {
                let fitted = fit(&p, hosted, own, Some(37)).unwrap();
                let at = |places| Bounds {
                    descriptors: p.descriptors,
                    places,
                    in_flight: p.threads,
                    hosted,
                };
                let places = fitted.limits.programs;
                let held = descriptors(at(places), own, Some(37));
                assert!(
                    held <= u64::try_from(p.descriptors).unwrap(),
                    "{p:?} {own:?} hosted {hosted}: {held} past {}",
                    p.descriptors
                );
                assert!(
                    places == p.programs
                        || descriptors(at(places + 1), own, Some(37))
                            > u64::try_from(p.descriptors).unwrap(),
                    "{p:?} {own:?} hosted {hosted}: {places} places, and one more fits"
                );
                assert_eq!(fitted.limits.connections, at(places).connections());
                assert_eq!(fitted.limits.in_flight, p.threads);
                assert_eq!(fitted.lanes, lanes(places));
            }
        }
    }
}

/// D251's admission comes from the node's own budgets: a hosted VM's lanes are its two control connections and, for
/// each program place, its three streams and its box's staging connection, with its controller's two descriptors. Each
/// lane holds what `hex_client::guest::lanes` counts and its tests hold to the real descriptors (the review's D-6): its
/// socket pair's two ends, a control connection's as every other's (the review's D-5). Do: read the lanes and the
/// hosted term at 10 places. Expect: 42 lanes, 86 descriptors.
#[test]
fn a_hosted_vms_lanes_are_its_control_connections_and_four_for_each_place() {
    assert_eq!(lanes(10), 2 + 10 * 4);
    assert_eq!(hosted_descriptors(1, 10), 42 * 2 + 2);
    assert_eq!(hosted_descriptors(0, 10), 0);
    let at = Bounds {
        descriptors: 1_000,
        places: 10,
        in_flight: 64,
        hosted: 1,
    };
    let named: Vec<_> = terms(at, MACOS, Some(0)).into_iter().collect();
    assert!(named.contains(&("the hosted VMs' lanes", 86)), "{named:?}");
}

/// A second hosted VM, should the form hold one, takes its lanes from the same descriptors: the places that fit
/// shrink, and each VM's lanes with them. Do: fit a tight platform for one hosted VM and for two. Expect: fewer places
/// for two, each VM's lanes those places', and both within the limit.
#[test]
fn a_second_hosted_vm_splits_the_lanes_term() {
    let p = platform(10_240, 1_000_000, 8_192);
    let one = fit(&p, 1, MACOS, Some(0)).unwrap();
    let two = fit(&p, 2, MACOS, Some(0)).unwrap();
    assert!(
        two.limits.programs < one.limits.programs,
        "{one:?} then {two:?}"
    );
    assert_eq!(two.lanes, lanes(two.limits.programs));
    for (fitted, hosted) in [(one, 1), (two, 2)] {
        let at = Bounds {
            descriptors: p.descriptors,
            places: fitted.limits.programs,
            in_flight: p.threads,
            hosted,
        };
        assert!(descriptors(at, MACOS, Some(0)) <= 10_240);
    }
}

/// A limit that can't hold the least node, one place and one connection, is refused naming what that node holds.
#[test]
fn a_limit_under_the_least_node_is_refused_naming_it() {
    let least = descriptors(
        Bounds {
            descriptors: 8,
            places: 1,
            in_flight: 1,
            hosted: 1,
        },
        WINDOWS,
        Some(4),
    );
    assert_eq!(fit(&platform(8, 1, 1), 1, WINDOWS, Some(4)), Err(least));
}

/// Before 2026-11-13 a macOS or Windows node's connections were its raw descriptor limit, counting nothing beside them.
/// Do: fit macOS's default soft limit with no hosted VM. Expect: fewer connections than descriptors, the program places'
/// streams and what was held at the start among the terms.
#[test]
fn a_nodes_connections_no_longer_take_its_whole_descriptor_limit() {
    let p = platform(10_240, 100_000, 100_000);
    let fitted = fit(&p, 0, MACOS, Some(12)).unwrap();
    assert!(fitted.limits.connections < p.descriptors, "{fitted:?}");
}
