//! The learned charge's core (joint plan step 14, AC-T7.4.1; `docs/design/learned-charge.md`): each test does X and
//! expects Y, and pins one number's basis.

use super::*;
use proptest::prelude::*;

const WINDOW: Duration = Duration::from_millis(100);
const GIB: u64 = 1 << 30;

fn res(cpu_milli: u64, mem_bytes: u64, disk_bytes: u64) -> Amounts {
    Amounts {
        cpu_milli,
        mem_bytes,
        disk_bytes,
    }
}

fn class_of(n: u8, cpu: u64, mem: u64) -> ClassKey {
    ClassKey::new(Class::Shards, [n; 32], res(cpu, mem, 0))
}

fn node(budget: Amounts) -> Node<u32> {
    Node::new(
        WINDOW,
        budget,
        Bounds {
            vms: 64,
            classes: 64,
        },
    )
    .unwrap()
}

/// The VM the last resort ends, if any.
fn ended(n: &Node<u32>) -> Option<u32> {
    match n.last_resort().unwrap() {
        Resort::End { vm, .. } => Some(vm),
        Resort::Held | Resort::Within(_) => None,
    }
}

/// One window's reading: `cpu_milli` of the window's CPU, `mem` held at most.
fn sample(cpu_milli: u64, mem: u64) -> Sample {
    Sample {
        cpu: Cpu::Whole(Duration::from_nanos(cpu_milli * 100_000)),
        span: WINDOW,
        mem_bytes: mem,
        disk_bytes: Some(0),
    }
}

fn window(node: &mut Node<u32>, readings: &[(u32, u64, u64)]) {
    for &(id, cpu, mem) in readings {
        node.observe(id, &sample(cpu, mem)).unwrap();
    }
    node.settle().unwrap();
}

/// waits_census.py's nearest rank of the p99.9, `ceil(0.999 n)`, in integers: `ceil(999 n / 1000)`. For every n up
/// to SEEDABLE the float product is within an ulp of the exact one, which is whole only at n = 1000 (999), so the two
/// agree.
fn rank(n: usize) -> usize {
    (999 * n).div_ceil(1000)
}

/// The definition: the nearest-rank p99.9 of the last SEEDABLE of `values`, as waits_census.py's `quantile` has it.
fn nearest_rank(values: &[u64]) -> u64 {
    let last = &values[values.len().saturating_sub(SEEDABLE)..];
    let mut sorted = last.to_vec();
    sorted.sort_unstable();
    sorted[rank(sorted.len()).max(1) - 1]
}

/// The same by a scan: the `n / SEEDABLE + 1`-th largest of the last SEEDABLE, equal values counted apart.
fn kth_largest(values: &[u64]) -> u64 {
    let last = &values[values.len().saturating_sub(SEEDABLE)..];
    let k = last.len() / SEEDABLE + 1;
    let first = *last.iter().max().unwrap();
    if k == 1 || last.iter().filter(|v| **v == first).count() >= 2 {
        first
    } else {
        *last.iter().filter(|v| **v < first).max().unwrap()
    }
}

/// Joint plan step 14 (AC-T7.4.1): SEEDABLE is decision 9's census rule. Do read waits_census.py's SEEDABLE;
/// expect this core's, and expect its claim: with SEEDABLE readings the nearest-rank p99.9 is the second largest,
/// an observed sample, and with one fewer it is the largest.
#[test]
fn w14_seedable_is_the_waits_census_rule() {
    let census = include_str!("../../../../bench/competitive/probes/waits_census.py");
    let seedable: usize = census
        .lines()
        .find_map(|line| line.strip_prefix("SEEDABLE = "))
        .map(|n| n.trim().replace('_', "").parse().unwrap())
        .unwrap();
    assert_eq!(seedable, SEEDABLE);
    assert_eq!(rank(SEEDABLE), SEEDABLE - 1);
    assert_eq!(rank(SEEDABLE - 1), SEEDABLE - 1);
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(16))]

    /// Joint plan step 14 (AC-T7.4.1), C1's tail: do push any readings into a tail, one dimension's of a few values
    /// so equal readings come in runs; expect each dimension's p99.9 after every push to be the `n / SEEDABLE +
    /// 1`-th largest of its last SEEDABLE readings, and that, at every 97th push and the last, the nearest-rank
    /// p99.9 as waits_census.py's `quantile` defines it.
    #[test]
    fn w14_the_tail_is_the_nearest_rank_p999_of_the_last_seedable_readings(
        readings in proptest::collection::vec((0u64..2_000, 0u64..1 << 20, 0u64..4), 1..2_600)
    ) {
        let mut tail = Tail::new().unwrap();
        let mut seen: [Vec<u64>; 3] = Default::default();
        let count = readings.len();
        for (at, (cpu, mem, disk)) in readings.into_iter().enumerate() {
            tail.push([cpu, mem, disk]).unwrap();
            seen[0].push(cpu);
            seen[1].push(mem);
            seen[2].push(disk);
            let p = tail.p999().unwrap();
            for d in 0..3 {
                prop_assert_eq!(p[d], kth_largest(&seen[d]), "dimension {} after {} readings", d, at + 1);
                if at % 97 == 0 || at + 1 == count {
                    prop_assert_eq!(p[d], nearest_rank(&seen[d]));
                }
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Joint plan step 14 (AC-T7.4.1), C4's guarantee: do share a capacity no smaller than the weights' sum among
    /// any weights and needs; expect each to be given at least the lesser of its need and its weight and never
    /// more than its need, and the whole capacity or every need given (work-conserving).
    #[test]
    fn w14_every_share_is_at_least_its_need_or_its_weight(
        entries in proptest::collection::vec((1u64..10_000, 0u64..100_000), 1..48),
        spare in 0u64..50_000,
    ) {
        let weights: u64 = entries.iter().map(|(w, _)| w).sum();
        let capacity = weights + spare;
        let given = shares(capacity, &entries).unwrap();
        for ((weight, need), got) in entries.iter().zip(&given) {
            prop_assert!(*got >= (*need).min(*weight), "{got} < min({need}, {weight})");
            prop_assert!(got <= need);
        }
        let needs: u64 = entries.iter().map(|(_, n)| n).sum();
        prop_assert_eq!(given.iter().sum::<u64>(), capacity.min(needs));
    }

    /// Joint plan step 14 (AC-T7.4.1), C3: do run warmed VMs over windows of any use; expect after every window
    /// the bookings within the budget, each at most its charge, and one short of its charge only where the budget
    /// (or, for memory, what VMs hold) left no room.
    #[test]
    fn w14_bookings_stay_within_the_budget_and_rise_only_into_room(
        uses in proptest::collection::vec(proptest::collection::vec((0u64..4_000, 0u64..8 * GIB), 4), 8),
    ) {
        let budget = res(8_000, 16 * GIB, 0);
        let mut n = node(budget);
        for id in 0..4u32 {
            prop_assert_eq!(n.admit(id, class_of(1, 2_000, 4 * GIB), 0, 2).unwrap(), res(2_000, 4 * GIB, 0));
        }
        let ids: Vec<u32> = (0..4).collect();
        let mut rounds = 0;
        for round in &uses {
            for _ in 0..(SEEDABLE / 4) {
                let readings: Vec<(u32, u64, u64)> = ids.iter().zip(round).map(|(id, (c, m))| (*id, *c, *m)).collect();
                window(&mut n, &readings);
                rounds += 1;
                let booked = ids.iter().map(|id| n.booking(id).unwrap()).fold(res(0, 0, 0), |a, b| a.checked_add(b).unwrap());
                prop_assert!(budget.holds(booked), "{booked:?} past {budget:?} after {rounds} windows");
                for id in &ids {
                    let (booking, charge) = (n.booking(id).unwrap(), n.charge(id).unwrap());
                    prop_assert!(charge.holds(booking), "{id}: booking {booking:?} past charge {charge:?}");
                }
                let room = n.room().unwrap();
                for (id, excess) in n.over() {
                    prop_assert!(
                        (excess.cpu_milli == 0 || room.cpu_milli == 0)
                            && (excess.mem_bytes == 0 || room.mem_bytes == 0),
                        "{id} over by {excess:?} with room {room:?}"
                    );
                }
            }
        }
    }
}

/// Joint plan step 14 (AC-T7.4.1), C1's cold rule: do admit a VM with no prior and read it at a tenth of a core;
/// expect it charged its allocation through its first SEEDABLE - 1 windows, then its own tail's p99.9, and its
/// booking down to it at once.
#[test]
fn w14_a_cold_vm_is_charged_its_booking_until_its_tail_is_full() {
    let mut n = node(res(4_000, 8 * GIB, 0));
    let class = class_of(1, 4_000, 4 * GIB);
    assert_eq!(n.admit(1, class, 0, 4).unwrap(), class.ceiling());
    for _ in 0..SEEDABLE - 1 {
        window(&mut n, &[(1, 100, GIB)]);
    }
    assert_eq!(n.warmed(&1), Some(false));
    assert_eq!(n.charge(&1).unwrap(), class.ceiling());
    window(&mut n, &[(1, 100, GIB)]);
    assert_eq!(n.warmed(&1), Some(true));
    assert_eq!(n.charge(&1).unwrap(), res(100, GIB, 0));
    assert_eq!(n.booking(&1).unwrap(), res(100, GIB, 0));
}

/// Joint plan step 14 (AC-T7.4.1), C1's ceiling (R03, learning never raises authority): do read a warmed VM past its
/// allocation in CPU and memory; expect its charge to stop at the allocation.
#[test]
fn w14_the_charge_never_passes_the_allocation() {
    let mut n = node(res(16_000, 64 * GIB, 0));
    let class = class_of(1, 2_000, 4 * GIB);
    n.admit(1, class, 0, 2).unwrap();
    for _ in 0..SEEDABLE {
        window(&mut n, &[(1, 5_000, 9 * GIB)]);
    }
    assert_eq!(n.charge(&1).unwrap(), class.ceiling());
}

/// Joint plan step 14 (AC-T7.4.1), C1's window: do read CPU over twice the window and over half of it; expect the
/// first charged over its span and the second over the whole window (never less), each rounded up.
#[test]
fn w14_a_reading_is_charged_over_its_span_never_less_than_the_window() {
    let mut n = node(res(16_000, 64 * GIB, 0));
    n.admit(1, class_of(1, 16_000, 4 * GIB), 0, 16).unwrap();
    let read = |n: &mut Node<u32>, cpu: Duration, span: Duration| {
        n.observe(
            1,
            &Sample {
                cpu: Cpu::Whole(cpu),
                span,
                mem_bytes: 0,
                disk_bytes: Some(0),
            },
        )
        .unwrap();
        n.vms[&1].held[0]
    };
    assert_eq!(read(&mut n, WINDOW, WINDOW * 2), 500);
    assert_eq!(read(&mut n, WINDOW / 2, WINDOW / 2), 500);
    assert_eq!(read(&mut n, Duration::from_nanos(1), WINDOW), 1);
    assert_eq!(read(&mut n, WINDOW * 3, WINDOW), 3_000);
}

/// Joint plan step 14 (AC-T7.4.1), C1's sample: do ask a whole reading for its vCPUs; expect the typed upstream
/// absence. Do read a split naming another vCPU count; expect it refused. Do read a split of two vCPUs and the
/// VMM's own; expect the charge their sum.
#[test]
fn w14_per_vcpu_guest_time_awaits_upstream() {
    assert_eq!(
        Cpu::Whole(WINDOW).per_vcpu(),
        Err(ChargeError::Awaits(Upstream::GuestCpuTimePerVcpu))
    );
    let mut n = node(res(16_000, 64 * GIB, 0));
    n.admit(1, class_of(1, 2_000, 4 * GIB), 0, 2).unwrap();
    let split = |guest: Vec<Duration>| Sample {
        cpu: Cpu::Split {
            guest,
            own: WINDOW / 10,
        },
        span: WINDOW,
        mem_bytes: 0,
        disk_bytes: Some(0),
    };
    assert!(matches!(
        n.observe(1, &split(vec![WINDOW])),
        Err(ChargeError::Invalid { .. })
    ));
    n.observe(1, &split(vec![WINDOW, WINDOW / 2])).unwrap();
    assert_eq!(n.vms[&1].held[0], 1_600);
    assert_eq!(
        split(vec![WINDOW, WINDOW / 2]).cpu.per_vcpu().unwrap(),
        &[WINDOW, WINDOW / 2]
    );
}

/// Joint plan step 14 (AC-T7.4.1), C2: do admit a VM of a class no VM has warmed; expect its allocation. Do warm one
/// whose windows are low but for two high; expect the class's prior the high one (the p99.9 of its last SEEDABLE
/// warmed windows) and a second VM booked at it. Do read SEEDABLE more low windows; expect the prior to fall to the
/// low, as the outlier's windows leave the sample.
#[test]
fn w14_the_prior_is_the_p999_of_the_class_last_warmed_windows_and_falls() {
    let mut n = node(res(16_000, 64 * GIB, 0));
    let class = class_of(1, 8_000, 8 * GIB);
    assert_eq!(n.prior(&class), class.ceiling());
    n.admit(1, class, 0, 8).unwrap();
    for t in 0..SEEDABLE {
        let cpu = if t == 10 || t == 20 { 3_000 } else { 200 };
        window(&mut n, &[(1, cpu, GIB)]);
    }
    assert_eq!(n.prior(&class), res(3_000, GIB, 0));
    assert_eq!(n.admit(2, class, 0, 8).unwrap(), res(3_000, GIB, 0));
    n.end(&2).unwrap();
    for _ in 0..SEEDABLE {
        window(&mut n, &[(1, 200, GIB)]);
    }
    assert_eq!(n.prior(&class), res(200, GIB, 0));
}

/// Joint plan step 14 (AC-T7.4.1), C6: do book a VM at a whole node's CPU and admit another; expect it refused,
/// typed, with its booking and the room (none). Do let a VM hold memory past its booking; expect the room and the
/// reservation to count what it holds.
#[test]
fn w14_admission_refuses_with_the_room_and_counts_memory_held_past_bookings() {
    let mut n = node(res(4_000, 16 * GIB, 0));
    let class = class_of(1, 4_000, 4 * GIB);
    n.admit(1, class, 0, 4).unwrap();
    assert_eq!(
        n.admit(2, class, 0, 4),
        Err(ChargeError::NoRoom {
            booking: class.ceiling(),
            room: res(0, 12 * GIB, 0),
        })
    );
    n.observe(1, &sample(100, 6 * GIB)).unwrap();
    assert_eq!(n.room().unwrap(), res(0, 10 * GIB, 0));
    assert_eq!(n.reservation().unwrap(), res(4_000, 6 * GIB, 0));
}

/// Joint plan step 14 (AC-T7.4.1), C4's weights: do weigh bookings of 4,000, 2,000, 1 and 0 milli-CPU; expect the
/// largest at the kernel's top weight (cgroup-v2.rst: 1 to 10,000), the rest its share rounded up, at least 1.
#[test]
fn w14_weights_put_the_largest_booking_at_the_kernels_top() {
    let mut n = node(res(16_000, 64 * GIB, 0));
    for (id, cpu) in [(1, 4_000), (2, 2_000), (3, 1), (4, 0)] {
        n.admit(id, class_of(u8::try_from(id).unwrap(), cpu, GIB), 0, 4)
            .unwrap();
    }
    assert_eq!(
        n.weights().unwrap(),
        vec![(1, WEIGHT_MAX), (2, 5_000), (3, 3), (4, WEIGHT_MIN)]
    );
}

/// Joint plan step 14 (AC-T7.4.1), C5's budgets: do book VMs at 1, 2 and 3 GiB of a 10 GiB budget, one holding
/// 6 GiB; expect budgets summing to the budget exactly, each its booking and its share of the slack in
/// proportion, and the room its budget less what it holds (negative for the one past it).
#[test]
fn w14_budgets_sum_to_the_fleet_budget_and_cover_every_booking() {
    let mut n = node(res(16_000, 10 * GIB, 0));
    for (id, mem) in [(1, GIB), (2, 2 * GIB), (3, 3 * GIB)] {
        n.admit(id, class_of(u8::try_from(id).unwrap(), 1_000, mem), 0, 1)
            .unwrap();
    }
    n.observe(3, &sample(0, 6 * GIB)).unwrap();
    let budgets = n.budgets().unwrap();
    assert_eq!(budgets.iter().map(|(_, b)| b.bytes).sum::<u64>(), 10 * GIB);
    let slack = 4 * GIB;
    for (id, budget) in &budgets {
        let booking = n.booking(id).unwrap().mem_bytes;
        let share = slack * booking / (6 * GIB);
        assert!(budget.bytes >= booking + share && budget.bytes <= booking + share + 1);
        let held = n.vms[id].held[1];
        assert_eq!(
            budget.room,
            i64::try_from(budget.bytes).unwrap() - i64::try_from(held).unwrap()
        );
    }
    assert!(budgets[2].1.room < 0);
}

/// Joint plan step 14 (AC-T7.4.1), C5's last resort: do book VMs at 2 GiB (band 1), 2 and 3 GiB (band 0) of a 10 GiB
/// budget and let the first hold 8 GiB and the second 3, each past its budget (its booking and its share of the
/// slack), the third within its; expect the band-0 VM past its budget ended first, then the band-1 one while the
/// rest still pass the fleet's budget, then none, and never the VM within its budget; the first end's why its own
/// memory against its budget, its band and the fleet's. With every VM within its budget but the growth on record past
/// the fleet's, expect none, said as within: growth is no fact yet.
#[test]
fn w14_the_last_resort_ends_only_a_vm_past_its_budget() {
    let mut n = node(res(16_000, 10 * GIB, 0));
    for (id, band, mem) in [(1, 1, 2 * GIB), (2, 0, 2 * GIB), (3, 0, 3 * GIB)] {
        n.admit(id, class_of(u8::try_from(id).unwrap(), 1_000, mem), band, 1)
            .unwrap();
    }
    for (id, held) in [(1, 8 * GIB), (2, 3 * GIB), (3, 3 * GIB)] {
        n.observe(id, &sample(0, held)).unwrap();
    }
    let budget = n
        .budgets()
        .unwrap()
        .into_iter()
        .find(|(id, _)| *id == 2)
        .unwrap()
        .1
        .bytes;
    assert_eq!(
        n.last_resort().unwrap(),
        Resort::End {
            vm: 2,
            why: Excess {
                held: 3 * GIB,
                budget,
                band: 0,
                fleet: Pressure {
                    held: 14 * GIB,
                    growth: 0,
                    budget: 10 * GIB,
                },
            },
        }
    );
    n.end(&2).unwrap();
    assert_eq!(ended(&n), Some(1));
    n.end(&1).unwrap();
    assert_eq!(n.last_resort().unwrap(), Resort::Held);

    let mut within = node(res(16_000, 10 * GIB, 0));
    for id in 1..=2 {
        within
            .admit(
                id,
                class_of(u8::try_from(id).unwrap(), 1_000, 5 * GIB),
                0,
                1,
            )
            .unwrap();
    }
    window(&mut within, &[(1, 0, 5 * GIB), (2, 0, 5 * GIB)]);
    assert_eq!(within.growth(), 10 * GIB);
    assert_eq!(
        within.last_resort().unwrap(),
        Resort::Within(Pressure {
            held: 10 * GIB,
            growth: 10 * GIB,
            budget: 10 * GIB,
        })
    );
}

/// Joint plan step 14 (AC-T7.4.1), C3's order in C5 (ROOT, 2026-11-12): within a band the later begun bears the
/// contention, so of VMs equally past their budgets the last resort ends the later. A node's VMs are keyed in the order
/// it began them (its learners key them by their records' places in its start order). Do admit VMs 1 then 2, one band,
/// at 2 GiB each in a 4 GiB budget, no slack left, and let each hold 3 GiB, each 1 GiB past its budget. Expect VM 2
/// ended.
#[test]
fn w14_of_vms_equally_past_their_budgets_the_later_begun_is_ended() {
    let mut n = node(res(16_000, 4 * GIB, 0));
    for id in [1, 2] {
        n.admit(
            id,
            class_of(u8::try_from(id).unwrap(), 1_000, 2 * GIB),
            0,
            1,
        )
        .unwrap();
    }
    for id in [1, 2] {
        n.observe(id, &sample(0, 3 * GIB)).unwrap();
    }
    assert_eq!(ended(&n), Some(2));
}

/// Joint plan step 14 (AC-T7.4.1), C5 as ROOT took it (2026-11-12): a VM `memory.high` holds within its budget is
/// never ended for growth to come. Do warm two VMs of 4 GiB allocations at 2 GiB each in an 8 GiB budget, so each is
/// booked at 2 GiB with a 4 GiB budget, and let the first hold 3 GiB, past its booking but within its budget, while the
/// growth on record (4 GiB, their first window) and what they hold pass the fleet's budget; expect none ended. Let
/// the first then hold 5 GiB, past its budget; expect it ended.
#[test]
fn w14_a_vm_held_within_its_budget_is_never_ended_for_growth_to_come() {
    let mut n = node(res(16_000, 8 * GIB, 0));
    n.admit(1, class_of(1, 1_000, 4 * GIB), 0, 1).unwrap();
    n.admit(2, class_of(2, 1_000, 4 * GIB), 0, 1).unwrap();
    for _ in 0..SEEDABLE {
        window(&mut n, &[(1, 0, 2 * GIB), (2, 0, 2 * GIB)]);
    }
    assert_eq!(n.booking(&1).unwrap().mem_bytes, 2 * GIB);
    assert_eq!(n.growth(), 4 * GIB);
    n.observe(1, &sample(0, 3 * GIB)).unwrap();
    let budgets = n.budgets().unwrap();
    assert_eq!(budgets[0].1.bytes, 4 * GIB);
    assert_eq!(budgets[0].1.room, i64::try_from(GIB).unwrap());
    assert!(matches!(n.last_resort().unwrap(), Resort::Within(_)));
    n.observe(1, &sample(0, 5 * GIB)).unwrap();
    assert_eq!(ended(&n), Some(1));
}

/// Joint plan step 14 (AC-T7.4.1), C5's growth: do grow two VMs' memory by 1 and 2 GiB in one window and by 1 GiB
/// in the next; expect the node's fastest growth on record 3 GiB, and it never falls.
#[test]
fn w14_the_fastest_growth_is_learned_from_the_windows() {
    let mut n = node(res(16_000, 64 * GIB, 0));
    n.admit(1, class_of(1, 1_000, 8 * GIB), 0, 1).unwrap();
    n.admit(2, class_of(2, 1_000, 8 * GIB), 0, 1).unwrap();
    window(&mut n, &[(1, 0, GIB), (2, 0, 2 * GIB)]);
    assert_eq!(n.growth(), 3 * GIB);
    window(&mut n, &[(1, 0, 2 * GIB), (2, 0, 2 * GIB)]);
    assert_eq!(n.growth(), 3 * GIB);
}

/// Joint plan step 14 (AC-T7.4.1), the classes' bound (CLAUDE.md §2): do hold two classes at a bound of two, end
/// the first's VM and admit a third class; expect the first forgotten (its prior the allocation again), the
/// second kept. With both held by live VMs, expect a third refused, typed.
#[test]
fn w14_classes_past_their_bound_forget_the_oldest_without_a_live_vm() {
    let mut n: Node<u32> = Node::new(
        WINDOW,
        res(16_000, 64 * GIB, 0),
        Bounds { vms: 8, classes: 2 },
    )
    .unwrap();
    let (a, b, c) = (
        class_of(1, 1_000, GIB),
        class_of(2, 1_000, GIB),
        class_of(3, 1_000, GIB),
    );
    n.admit(1, a, 0, 1).unwrap();
    n.admit(2, b, 0, 1).unwrap();
    for _ in 0..SEEDABLE {
        window(&mut n, &[(1, 10, GIB / 2), (2, 10, GIB / 2)]);
    }
    assert_eq!(n.prior(&a), res(10, GIB / 2, 0));
    assert!(matches!(n.admit(3, c, 0, 1), Err(ChargeError::Full { .. })));
    n.end(&1).unwrap();
    n.admit(3, c, 0, 1).unwrap();
    assert_eq!(n.prior(&a), a.ceiling());
    assert_eq!(n.prior(&b), res(10, GIB / 2, 0));
}

/// Joint plan step 14 (AC-T7.4.1), C4 against a planted bug: do warm a VM of three CPUs' allocation down to a booking
/// of one, book two more at one each, and share 3,000 milli-CPU while the first asks for all of it; expect each
/// neighbour its 1,000. Planted, CPU shared by what each asks: expect a neighbour given less than its booking, which
/// the guarantee catches.
#[test]
fn w14_the_guarantee_catches_cpu_shared_by_demand() {
    let mut n = node(res(3_000, 64 * GIB, 0));
    n.admit(1, class_of(1, 3_000, GIB), 0, 3).unwrap();
    for _ in 0..SEEDABLE {
        window(&mut n, &[(1, 1_000, GIB)]);
    }
    assert_eq!(n.booking(&1).unwrap().cpu_milli, 1_000);
    for id in 2..=3 {
        n.admit(id, class_of(2, 1_000, GIB), 0, 1).unwrap();
    }
    let demands = BTreeMap::from([(1, 3_000), (2, 1_000), (3, 1_000)]);
    let held = |given: &[(u32, u64)]| given.iter().all(|(id, got)| *id == 1 || *got >= 1_000);
    assert!(held(&n.shares(3_000, &demands).unwrap()));
    let mutated = with_mutant(Some(Mutant::WeightsByDemand), || {
        n.shares(3_000, &demands).unwrap()
    });
    assert!(!held(&mutated), "{mutated:?}");
}

/// Joint plan step 14 (AC-T7.4.1), C3 against a planted bug: do warm two VMs whose charges together pass the
/// budget; expect the bookings within it. Planted, bookings raised whatever the room: expect them past it.
#[test]
fn w14_bookings_past_the_room_are_caught() {
    let run = || {
        let mut n = node(res(4_000, 64 * GIB, 0));
        for id in 1..=2 {
            n.admit(id, class_of(1, 4_000, GIB), 0, 4)
                .unwrap_or_default();
        }
        for _ in 0..SEEDABLE {
            window(&mut n, &[(1, 100, GIB / 4)]);
        }
        n.admit(2, class_of(1, 4_000, GIB), 0, 4).unwrap();
        for _ in 0..SEEDABLE {
            window(&mut n, &[(1, 3_000, GIB / 4), (2, 3_000, GIB / 4)]);
        }
        n.booking(&1).unwrap().cpu_milli + n.booking(&2).unwrap().cpu_milli
    };
    assert!(run() <= 4_000);
    assert!(with_mutant(Some(Mutant::BookPastRoom), run) > 4_000);
}

/// Joint plan step 14 (AC-T7.4.1), C5 against a planted bug: do let a VM booked at 1 GiB hold 4 GiB and one booked at
/// 6 GiB hold its booking, past a 10 GiB budget; expect the first ended, never the second, within its booking.
/// Planted, the largest holder ended: expect the second chosen, which the rule refuses.
#[test]
fn w14_ending_a_vm_within_its_booking_is_caught() {
    let mut n = node(res(16_000, 10 * GIB, 0));
    for (id, mem) in [(1, GIB), (2, 6 * GIB)] {
        n.admit(id, class_of(u8::try_from(id).unwrap(), 1_000, mem), 0, 1)
            .unwrap();
    }
    for (id, held) in [(1, 4 * GIB + 1), (2, 6 * GIB)] {
        n.observe(id, &sample(0, held)).unwrap();
    }
    assert_eq!(ended(&n), Some(1));
    let mutated = with_mutant(Some(Mutant::EndLargestHolder), || ended(&n));
    assert_eq!(mutated, Some(2));
}

/// Joint plan step 14 (AC-T7.4.1), C4: a fleet's weight among its parent's children is its CPU budget on the machine's
/// scale, the whole machine the kernel's top weight. Do weigh budgets against machines; expect the share rounded up,
/// never under the kernel's least, a budget past the machine weighing it whole, and a machine with no CPU refused.
#[test]
fn w14_a_fleet_weighs_its_cpu_budget_on_the_machines_scale() {
    assert_eq!(fleet_weight(64_000, 64_000), Ok(WEIGHT_MAX));
    assert_eq!(fleet_weight(32_000, 64_000), Ok(5_000));
    assert_eq!(fleet_weight(1_000, 3_000), Ok(3_334));
    assert_eq!(fleet_weight(1, 64_000), Ok(1));
    assert_eq!(fleet_weight(0, 64_000), Ok(WEIGHT_MIN));
    assert_eq!(fleet_weight(128_000, 64_000), Ok(WEIGHT_MAX));
    assert!(matches!(
        fleet_weight(1_000, 0),
        Err(ChargeError::Invalid { .. })
    ));
}

/// Joint plan step 14 (AC-T7.4.1), C4 where a platform caps CPU rather than weighs it (macOS, Windows). Do track three
/// VMs of 8-CPU ceilings booked at 4, 2 and 1 CPUs in a 10-CPU fleet budget, the first having used its whole 4-CPU
/// cap, the second 0.5 CPU of its 3, the third no window yet, and cap them. Expect the second capped at its booking (it
/// asked less), the first and third sharing the 3 CPUs the bookings leave by their bookings (6.4 and 1.6 CPUs), the
/// caps summing to the budget; and VMs booked nothing in a budget of one milli-CPU held at the platform's least.
#[test]
fn w14_a_capping_platforms_rates_keep_every_booking_and_share_the_rest() {
    let mut n = node(res(10_000, 64 * GIB, 0));
    for (id, booking) in [(1, 4_000), (2, 2_000), (3, 1_000)] {
        n.track(
            id,
            class_of(u8::try_from(id).unwrap(), 8_000, 8 * GIB),
            0,
            8,
            res(booking, GIB, 0),
        )
        .unwrap();
    }
    let used = BTreeMap::from([(1, (4_000, 4_000)), (2, (500, 3_000))]);
    let rates = n.rates(10_000, &used, 10).unwrap();
    assert_eq!(rates, vec![(1, 6_400), (2, 2_000), (3, 1_600)]);
    let mut none = node(res(1, 64 * GIB, 0));
    for id in 1..=3 {
        none.track(
            id,
            class_of(u8::try_from(id).unwrap(), 8_000, 8 * GIB),
            0,
            8,
            res(0, GIB, 0),
        )
        .unwrap();
    }
    let idle = BTreeMap::from([(1, (0, 1)), (2, (0, 1)), (3, (0, 1))]);
    let least = none.rates(1, &idle, 10).unwrap();
    assert!(least.iter().all(|(_, cap)| *cap == 10), "{least:?}");
}

proptest! {
    /// Joint plan step 14 (AC-T7.4.1), C4 where a platform caps CPU: do cap up to eight VMs of any ceilings, booked
    /// within a fleet budget, each having used any CPU against any cap; expect every cap at least its booking and at
    /// most its ceiling (or the platform's least), and the caps within the budget but for those lifted to the least.
    #[test]
    fn w14_capped_rates_keep_every_booking_within_the_budget(
        vms in proptest::collection::vec((1u64..=16_000, 0u64..=1_000, 0u64..=32_000, 1u64..=32_000), 1..=8),
        capacity in 1u64..=128_000,
    ) {
        let mut n = node(res(capacity, 64 * GIB, 0));
        let total: u64 = vms.iter().map(|(ceiling, part, _, _)| ceiling * part / 1_000).sum();
        let mut used = BTreeMap::new();
        for (k, (ceiling, part, ran, cap)) in vms.iter().enumerate() {
            let id = u32::try_from(k).unwrap();
            // Bookings scaled to fit the budget (C3's invariant), each within its ceiling.
            let booking = (ceiling * part / 1_000) * capacity.min(total.max(1)) / total.max(1);
            n.track(id, class_of(u8::try_from(k).unwrap(), *ceiling, GIB), 0, 1, res(booking, GIB, 0)).unwrap();
            used.insert(id, (*ran, *cap));
        }
        let rates = n.rates(capacity, &used, 10).unwrap();
        let mut lifted = 0;
        for (id, cap) in &rates {
            let booking = n.booking(id).unwrap().cpu_milli;
            let ceiling = vms[usize::try_from(*id).unwrap()].0;
            prop_assert!(*cap >= booking.min(ceiling), "{} under its booking {}", cap, booking);
            prop_assert!(*cap <= ceiling.max(10), "{} past its ceiling {}", cap, ceiling);
            if *cap == 10 {
                lifted += 10;
            }
        }
        prop_assert!(rates.iter().map(|(_, cap)| cap).sum::<u64>() <= capacity + lifted);
    }
}

/// Joint plan step 14 (AC-T7.4.1), C4 across the node's cgroup: the fleet takes the units the other children leave.
/// Do take units from 32-CPU machines; expect the units left and the CPU they carry rounded down, the least weight and
/// no CPU once the others take the whole scale or more, and a machine with no CPU refused.
#[test]
fn w14_the_fleet_takes_the_units_the_boxes_leave() {
    assert_eq!(fleet_room(0, 32_000), Ok((WEIGHT_MAX, 32_000)));
    // A 4-CPU box (1,250), one booked nothing (1) and a 2-CPU OCI box (625): 8,124 units, 25,996.8 milli-CPU.
    assert_eq!(fleet_room(1_876, 32_000), Ok((8_124, 25_996)));
    assert_eq!(fleet_room(10_000, 32_000), Ok((WEIGHT_MIN, 0)));
    assert_eq!(fleet_room(12_000, 32_000), Ok((WEIGHT_MIN, 0)));
    assert!(matches!(fleet_room(0, 0), Err(ChargeError::Invalid { .. })));
}

proptest! {
    /// Joint plan step 14 (AC-T7.4.1), C4 across the node's cgroup: do weigh up to eight boxes by any bookings, some
    /// booked nothing, the fleet budgeted the CPU the units they leave carry (`fleet_room`), within the machine less
    /// their bookings, give each any need and share the machine as the kernel does; expect the weights within the
    /// scale's top and every box and the fleet given at least min(its need, its booking or budget).
    #[test]
    fn w14_the_fleet_in_the_units_left_keeps_every_childs_booking(
        machine in 1_000u64..=1_024_000,
        parts in proptest::collection::vec(0u64..=1_000, 1..=8),
        needs in proptest::collection::vec(0u64..=1_024_000, 9),
    ) {
        // Each box books up to a sixteenth of the machine, so together they leave the fleet some of the scale.
        let bookings: Vec<u64> = parts.iter().map(|p| machine / 16 * p / 1_000).collect();
        let weights: Vec<u64> = bookings.iter().map(|b| u64::from(fleet_weight(*b, machine).unwrap())).collect();
        let (rest, carried) = fleet_room(weights.iter().sum(), machine).unwrap();
        let budget = (machine - bookings.iter().sum::<u64>()).min(carried);
        let fleet = u64::from(fleet_weight(budget, machine).unwrap());
        prop_assert!(fleet <= u64::from(rest));
        prop_assert!(weights.iter().sum::<u64>() + fleet <= u64::from(WEIGHT_MAX));
        let mut entries = vec![(fleet, needs[0].min(budget))];
        entries.extend(weights.iter().zip(&needs[1..]).map(|(w, n)| (*w, *n)));
        let given = shares(machine, &entries).unwrap();
        prop_assert!(given[0] >= budget.min(needs[0]));
        for ((booking, need), got) in bookings.iter().zip(&needs[1..]).zip(&given[1..]) {
            prop_assert!(*got >= (*booking).min(*need), "{} for booking {}, need {}", got, booking, need);
        }
    }

    /// Joint plan step 14 (AC-T7.4.1), C4: do weigh two budgets against one machine; expect each weight never under
    /// its budget's share of the kernel's range and over it by less than one unit (or the least weight), and the
    /// larger budget never weighing less.
    #[test]
    fn w14_a_fleet_weight_covers_its_budgets_share(machine in 1u64..=1_024_000, a in 0u64..=2_048_000, b in 0u64..=2_048_000) {
        let (lo, hi) = (a.min(b), a.max(b));
        let (light, heavy) = (fleet_weight(lo, machine).unwrap(), fleet_weight(hi, machine).unwrap());
        prop_assert!(light <= heavy);
        for (budget, weight) in [(lo, light), (hi, heavy)] {
            prop_assert!((WEIGHT_MIN..=WEIGHT_MAX).contains(&weight));
            let share = u128::from(budget.min(machine)) * u128::from(WEIGHT_MAX);
            prop_assert!(u128::from(weight) * u128::from(machine) >= share);
            prop_assert!(weight == WEIGHT_MIN || (u128::from(weight) - 1) * u128::from(machine) < share);
        }
    }
}

/// Joint plan step 14 (AC-T7.4.1), C4 and C5: the bounds that hold the rule. Do place three cold VMs (booked at their
/// ceilings) on a machine of twice the fleet's CPU budget; expect the parent capped at the budget and weighing half
/// the machine, each VM weighed by its booking and capped at its ceiling, its budget past its ceiling held at the
/// ceiling. Then warm them below their ceilings and narrow the fleet's memory budget; expect each held at its budget,
/// the budgets summing to the fleet's. A fleet with no CPU budget is capped at the kernel's least quota. Every bound
/// is one the kernel holds.
#[test]
fn w14_the_placement_caps_the_fleet_at_its_budget_and_holds_each_vm_by_its_booking_budget_and_ceiling()
 {
    let ceilings = [(1, 1_000, GIB), (2, 2_000, 2 * GIB), (3, 4_000, 3 * GIB)];
    let mut n = node(res(16_000, 10 * GIB, 0));
    for (id, cpu, mem) in ceilings {
        n.admit(id, class_of(u8::try_from(id).unwrap(), cpu, mem), 0, 1)
            .unwrap();
    }
    let vm = |weight: u16, cpu: u64, high: u64, max: u64| hex_api::Bounds {
        cpu_weight: Some(weight),
        cpu_max_milli: Some(u32::try_from(cpu).unwrap()),
        memory_high: Some(high),
        memory_max: Some(max),
        ..hex_api::Bounds::default()
    };
    let placed = n.placement(32_000).unwrap();
    assert_eq!(
        placed.fleet,
        hex_api::Bounds {
            cpu_weight: Some(5_000),
            cpu_max_milli: Some(16_000),
            ..hex_api::Bounds::default()
        }
    );
    assert_eq!(
        placed.vms,
        vec![
            (1, vm(2_500, 1_000, GIB, GIB)),
            (2, vm(5_000, 2_000, 2 * GIB, 2 * GIB)),
            (3, vm(WEIGHT_MAX, 4_000, 3 * GIB, 3 * GIB)),
        ]
    );
    for _ in 0..SEEDABLE {
        window(
            &mut n,
            &[(1, 500, GIB / 2), (2, 1_000, GIB), (3, 2_000, 2 * GIB)],
        );
    }
    n.set_budget(res(16_000, 5 * GIB, 0));
    let placed = n.placement(32_000).unwrap();
    let budgets: BTreeMap<u32, Budget> = n.budgets().unwrap().into_iter().collect();
    let mut held = 0;
    for ((id, bounds), (_, cpu, mem)) in placed.vms.iter().zip(ceilings) {
        let budget = budgets[id].bytes;
        assert!(
            budget < mem,
            "VM {id}'s budget {budget} is under its ceiling"
        );
        assert!(budget >= n.booking(id).unwrap().mem_bytes);
        assert_eq!(
            *bounds,
            vm(
                [2_500, 5_000, WEIGHT_MAX][usize::try_from(*id - 1).unwrap()],
                cpu,
                budget,
                mem
            )
        );
        held += budget;
    }
    assert_eq!(held, 5 * GIB);
    n.set_budget(res(0, 5 * GIB, 0));
    let placed = n.placement(32_000).unwrap();
    assert_eq!(placed.fleet.cpu_max_milli, Some(hex_api::MIN_CPU_MILLI));
    assert_eq!(placed.fleet.cpu_weight, Some(WEIGHT_MIN));
    for bounds in std::iter::once(&placed.fleet).chain(placed.vms.iter().map(|(_, b)| b)) {
        assert!(
            bounds.check().is_ok(),
            "{bounds:?} is no bound the kernel holds"
        );
    }
}

/// Joint plan step 14 (AC-T7.4.1), C4 across the node's cgroup: every child on one scale. Do weigh the VM fleet by its
/// budget (the machine less the boxes' bookings) and each process box by its booking, all with `fleet_weight`, and
/// share a 32-CPU machine among them as the kernel does (`shares`) while all but one box ask for the whole machine;
/// expect each box given at least min(its need, its booking), and the fleet at least its budget.
#[test]
fn w14_a_process_box_under_contention_gets_at_least_its_booking() {
    let machine = 32_000;
    let boxes = [(4_000, 32_000), (2_000, 1_000), (6_000, 32_000)];
    let budget = machine - boxes.iter().map(|(booking, _)| booking).sum::<u64>();
    let mut entries = vec![(u64::from(fleet_weight(budget, machine).unwrap()), machine)];
    for (booking, need) in boxes {
        entries.push((u64::from(fleet_weight(booking, machine).unwrap()), need));
    }
    assert_eq!(
        entries.iter().map(|(weight, _)| weight).sum::<u64>(),
        u64::from(WEIGHT_MAX)
    );
    let given = shares(machine, &entries).unwrap();
    assert!(
        given[0] >= budget,
        "the fleet given {} of {budget}",
        given[0]
    );
    for ((booking, need), got) in boxes.iter().zip(&given[1..]) {
        assert!(
            *got >= (*booking).min(*need),
            "{got} for booking {booking}, need {need}"
        );
    }
}

proptest! {
    /// Joint plan step 14 (AC-T7.4.1), C4 across the node's cgroup: do weigh up to eight boxes by bookings in whole
    /// units of the scale and the fleet by what they leave, give each any need, and share the machine as the kernel
    /// does; expect every box and the fleet given at least min(its need, its booking or budget).
    #[test]
    fn w14_every_child_on_one_scale_gets_its_booking(
        units in proptest::collection::vec(1u64..=1_249, 1..=8),
        needs in proptest::collection::vec(0u64..=40_000, 9),
    ) {
        // A machine of 40 CPUs: one unit of the scale is 4 milli-CPU, so each booking's weight is exact, and the boxes
        // leave the fleet at least a unit, so the weights sum to the scale's top.
        let machine = 40_000;
        let unit = machine / u64::from(WEIGHT_MAX);
        let budget = machine - units.iter().sum::<u64>() * unit;
        let mut entries = vec![(u64::from(fleet_weight(budget, machine).unwrap()), needs[0])];
        for (k, need) in units.iter().zip(&needs[1..]) {
            entries.push((u64::from(fleet_weight(k * unit, machine).unwrap()), *need));
        }
        let given = shares(machine, &entries).unwrap();
        prop_assert!(given[0] >= budget.min(needs[0]));
        for ((k, need), got) in units.iter().zip(&needs[1..]).zip(&given[1..]) {
            prop_assert!(*got >= (k * unit).min(*need), "{} for booking {}, need {}", got, k * unit, need);
        }
    }
}
