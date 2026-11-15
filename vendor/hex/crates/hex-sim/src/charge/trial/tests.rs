//! A hardware run's reckoning (joint plan step 14, AC-T7.4.1; `docs/design/learned-charge.md`, "what ROOT measures").

use super::*;
use crate::charge::SEEDABLE;

fn reading(vm: u32, asked: u64, given: u64, booked: u64) -> Reading {
    Reading {
        vm,
        asked,
        given,
        booked,
    }
}

/// Joint plan step 14 (AC-T7.4.1): what a run's windows say of C4's guarantee. Do reckon a window within its 4-CPU
/// budget where vm 1 is given less than it asked, one contended where every VM is given at least min(asked, booked),
/// and one contended where vm 1, booked 2 CPUs and asking 3, is given 1.5 while the bookings fit. Expect two contended
/// windows; their four ratios, from 750 per mille (vm 1's short window) up; one miss, vm 1's in the contended window,
/// the uncontended window's none; and Jain's index of the contended windows, the least the uneven one's.
#[test]
fn w14_a_runs_windows_say_where_the_guarantee_held() {
    let windows = vec![
        Window {
            budget: 4_000,
            readings: vec![
                reading(1, 1_000, 900, 2_000),
                reading(2, 1_000, 1_000, 2_000),
            ],
        },
        Window {
            budget: 4_000,
            readings: vec![
                reading(1, 3_000, 2_000, 2_000),
                reading(2, 3_000, 2_000, 2_000),
            ],
        },
        Window {
            budget: 4_000,
            readings: vec![
                reading(1, 3_000, 1_500, 2_000),
                reading(2, 3_000, 2_500, 2_000),
            ],
        },
    ];
    let report = report(&Cast::default(), &windows);
    assert_eq!((report.windows, report.contended, report.misses), (3, 2, 1));
    assert_eq!(report.ratios.n, 4);
    assert_eq!(report.ratios.least, 750);
    assert_eq!(report.ratios.p50, 1_000);
    // (1.5/3 + 2.5/3)^2 / (2 ((1.5/3)^2 + (2.5/3)^2)), the uneven window's: 941,176 per million.
    assert_eq!(report.jain_least_ppm, 941_176);
    assert_eq!(report.jain_mean_ppm, (1_000_000 + 941_176) / 2);
}

/// Joint plan step 14 (AC-T7.4.1): the run's own floor, what its VMs were given of what they asked where nothing
/// contended. Do reckon a window within its 4-CPU budget where vm 1 is given 900 of the 1,000 milli-CPU it asked, vm 2
/// all of its 1,000 and vm 3, asking nothing, 50; then a contended window where vm 1 is given 1,500 of 3,000. Expect the
/// floor of the uncontended window's two VM-windows that asked, 900 per mille the least and, by nearest rank of two, its
/// p1 and p50: the contended window's and the VM asking nothing none of it.
#[test]
fn w14_a_runs_floor_is_what_its_uncontended_windows_gave_of_what_they_asked() {
    let windows = vec![
        Window {
            budget: 4_000,
            readings: vec![
                reading(1, 1_000, 900, 2_000),
                reading(2, 1_000, 1_000, 2_000),
                reading(3, 0, 50, 2_000),
            ],
        },
        Window {
            budget: 4_000,
            readings: vec![
                reading(1, 3_000, 1_500, 2_000),
                reading(2, 3_000, 2_500, 2_000),
            ],
        },
    ];
    let report = report(&Cast::default(), &windows);
    assert_eq!(
        report.floor,
        Ratios {
            n: 2,
            least: 900,
            p1: 900,
            p50: 900,
        }
    );
    assert_eq!(report.ratios.n, 2);
}

/// Joint plan step 14 (AC-T7.4.1): a guarantee that can't hold isn't missed. Do reckon a contended window whose bookings
/// pass its budget, vm 1 given less than it is booked. Expect no miss: C4 keeps each VM's booking only while the
/// bookings fit.
#[test]
fn w14_a_window_booked_past_its_budget_misses_nothing() {
    let windows = vec![Window {
        budget: 2_000,
        readings: vec![
            reading(1, 2_000, 1_000, 2_000),
            reading(2, 2_000, 1_000, 2_000),
        ],
    }];
    let report = report(&Cast::default(), &windows);
    assert_eq!((report.contended, report.misses), (1, 0));
}

/// Joint plan step 14 (AC-T7.4.1): the riser and its neighbours. Do reckon a run of a 6-CPU budget cast with vm 3 the
/// riser from its second window and vm 9 the process box: in the first window, uncontended, vm 1 is given less than it
/// asked; in the second, contended, vm 2 is short and so is the box, while vm 3 asks 4 CPUs past its 1-CPU booking and
/// is given 2. Expect two misses, both the second window's: one neighbour short (vm 2's, from the rise) and the box
/// short once; and the riser over in its one window, given half of what it asked.
#[test]
fn w14_a_runs_riser_and_its_neighbours() {
    let windows = vec![
        Window {
            budget: 6_000,
            readings: vec![
                reading(1, 1_000, 500, 1_000),
                reading(2, 1_000, 1_000, 1_000),
                reading(3, 500, 500, 1_000),
                reading(9, 1_000, 1_000, 1_000),
            ],
        },
        Window {
            budget: 6_000,
            readings: vec![
                reading(1, 1_000, 1_000, 1_000),
                reading(2, 1_000, 900, 1_000),
                reading(3, 4_000, 2_000, 1_000),
                reading(9, 1_000, 800, 1_000),
            ],
        },
    ];
    let cast = Cast {
        riser: Some((3, 1)),
        boxed: Some(9),
    };
    let report = report(&cast, &windows);
    assert_eq!(report.misses, 2);
    assert_eq!(report.neighbours_short, 1);
    assert_eq!(report.box_short, 1);
    assert_eq!((report.riser_windows, report.riser_over), (1, 1));
    assert_eq!(report.riser_given_ppm, 500_000);
}

/// Joint plan step 14 (AC-T7.4.1), C1's falsifier: a window past its VM's charge. Do count a VM's windows of 100
/// milli-CPU for SEEDABLE windows, then one of 100, one of 101, and one of 50. Expect three windows counted, those after
/// the first SEEDABLE, and one past the charge, the window of 101: the p99.9 of SEEDABLE windows of 100 is 100.
#[test]
fn w14_a_window_past_its_charge_is_a_violation() {
    let mut uses = vec![100u64; SEEDABLE];
    uses.extend([100, 101, 50]);
    assert_eq!(violations(&uses).unwrap(), (1, 3));
    assert_eq!(violations(&uses[..SEEDABLE]).unwrap(), (0, 0));
}
