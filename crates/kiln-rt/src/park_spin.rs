//! A spin before each park, its length sized from the shard's own idle gaps (kiln's goal 9, plan 01a1a87b2cec step 4:
//! keep the host off the shards).
//!
//! **Why.** Under Linux's lazy preemption (kiln's docs/priority.md, "The kernel's preemption") a shard woken from
//! another CPU gets its CPU, where a thread busy in user space holds it, only at that CPU's next interrupt: the tick at
//! the latest, up to 1 ms at 1000 Hz. Every fair-class wake is lazy there, so nothing a normal thread may set makes the
//! wake itself preempt; the nudge bounds the wait with timers of the shard's own (`crate::compute::nudge_current_thread`).
//! A shard still spinning when its work comes is never woken at all: its senders find it running and skip their kicks
//! (`crate::parking`), and the work runs at once. The spin costs the CPU it spins, which the shard's busy share counts
//! (kiln's goal 9 step 5 holds that share to a tenth of goal 3's: 0.011 calm, 0.0056 with hogs), so the spin is only
//! as long as the shard's own gaps make worth it in CPU.
//!
//! **The gap.** An idle moment's gap runs from the shard going idle to its next work. A spin that finds work measures it
//! exactly. A park measures it to the kick's stamp where a kick ended the wait (the sender stamps it,
//! [`crate::parking::Parking::time_wakes`], which a sizing shard turns on), since the park's own length adds the
//! wake's latency, a lazy wake's up to a tick; else to the wait's return (a socket's readiness, a timer's deadline: no
//! stamp, so its gap reads long, the cautious side).
//!
//! **The size.** After each [`WINDOW`] idle moments, the spin becomes the one of 0 and the park bounds up to the asked
//! cap (`crate::shard_loop::PARK_BOUNDS_NS`: 1, 2, 4, 8 and 16 µs for kiln's) that would have spent the least of the
//! shard's CPU on the window's gaps ([`cheapest`]): a gap the spin catches costs the spin up to it, taken at its
//! bound's top (the cautious side); one it misses costs the whole spin and the park after it. What a park costs is
//! learned from the parks the shard pays (`crate::park_cost`: its thread's CPU across each wait on its task clock,
//! no system call, and its senders' kicks): a spin of 0 costs each gap one. So the shard spins only where, on the last
//! window's gaps, spinning spent less CPU than parking at once, and parks at once where nothing would: spin-then-block
//! [A: Karlin, Li, Manasse, Owicki, "Empirical studies of competitive spinning for a shared-memory multiprocessor",
//! SOSP 1991], its threshold fitted to the gaps measured rather than to the worst case. Its spins, their hits and their
//! time are counted (`Counters::park_spins`, `park_spin_hits`, `park_spin_ns`), and the spin in force
//! (`park_spin_now_ns`), so a lab weighs them against the busy share.
//!
//! A fixed spin ([`Ask::Fixed`]) spends what it spends: a lab's measure of what a longer spin would buy, and cost.

use std::cell::Cell;

use crate::RtError;
use crate::parking::Woken;
use crate::shard_loop::PARK_BOUNDS_NS;

/// What a shard's thread asks of its spin before each park ([`ask_current_thread`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Ask {
    /// No spin: each idle moment parks at once, as without this module.
    #[default]
    Off,
    /// A spin as long as the shard's own gaps make cheapest in CPU (the module notes), at most `cap_ns`: one of 0 and
    /// the park bounds up to it.
    Sized {
        /// The longest spin, nanoseconds.
        cap_ns: u64,
    },
    /// A spin of `ns` at every idle moment, whatever it costs.
    Fixed {
        /// The spin, nanoseconds.
        ns: u64,
    },
}

/// The cap kiln asks its shards' sized spin for, nanoseconds: 16 µs, the first of the park bounds past the cheapest
/// wake the lab measured on fedora (a socket's wake on the waiting thread's own CPU, 12.6 µs at its median: the wake-2
/// lab, 2026-11-05), so no spin outlasts what that wake would have taken (`PARK_BOUNDS_NS`' notes).
pub const CAP_NS: u64 = 16_000;

/// Shape: the idle moments each sizing reads: as many as hold a share of them (the gaps under a bound) within ±5 points
/// at 95 % at a share's widest, ½, the runtime's own bar for what it measures
/// (`crate::machine::stats::CONVERGED_WIDTH_PERMILLE`, ±5 %, at the 95 % quantile, 1.960):
/// `⌈(1.960 · ½ / 0.05)²⌉ = ⌈384.16⌉ = 385`.
pub const WINDOW: u64 = 385;

thread_local! {
    /// The calling thread's ask: set by [`ask_current_thread`] (a task on the shard), read by its loop at each idle
    /// moment. A thread's own, as its nudge is (`crate::prioritysys`).
    static ASKED: Cell<Ask> = const { Cell::new(Ask::Off) };
}

/// Asks the shard the calling task runs on to spin before its parks as `ask` says (kiln's goal 9, plan 01a1a87b2cec
/// step 4). Refused off a shard's thread.
pub fn ask_current_thread(ask: Ask) -> Result<(), RtError> {
    crate::registry::with_current(|_| ()).ok_or(RtError::NotOnShardThread)?;
    ASKED
        .try_with(|asked| asked.set(ask))
        .map_err(|_| RtError::NotOnShardThread)
}

/// The calling thread's ask; [`Ask::Off`] on a thread that is ending.
#[inline]
pub(crate) fn asked() -> Ask {
    ASKED.try_with(Cell::get).unwrap_or(Ask::Off)
}

/// Of 0 and the park bounds up to `cap_ns`, the spin that would have spent the least CPU on `gaps` idle moments, of
/// whose gaps `under[i]` fell under `PARK_BOUNDS_NS[i]` (a histogram's cumulative buckets), a park costing `park_ns`:
/// a gap a spin catches costs the spin up to it, taken at its bound's top; one it misses costs the whole spin and a
/// park; with no spin, each costs a park. The shorter of two that cost the same.
pub fn cheapest(under: &[u64; PARK_BOUNDS_NS.len()], gaps: u64, cap_ns: u64, park_ns: u64) -> u64 {
    let mut best = (gaps.saturating_mul(park_ns), 0);
    // What the gaps under the bound so far cost the spin that catches them, and how many those were.
    let (mut caught, mut below) = (0u64, 0u64);
    for (bound, under) in PARK_BOUNDS_NS.iter().zip(under) {
        if *bound > cap_ns {
            break;
        }
        caught = caught.saturating_add(under.saturating_sub(below).saturating_mul(*bound));
        below = (*under).max(below);
        let missed = gaps.saturating_sub(below);
        let cost = caught.saturating_add(missed.saturating_mul(bound.saturating_add(park_ns)));
        if cost < best.0 {
            best = (cost, *bound);
        }
    }
    best.1
}

/// The loop's side of the spin: what it was asked, the spin in force, and the window that sizes it.
#[derive(Debug, Default)]
pub(crate) struct ParkSpin {
    ask: Ask,
    spin_ns: u64,
    /// The window's idle moments, and how many of their gaps fell under each park bound (cumulative).
    gaps: u64,
    under: [u64; PARK_BOUNDS_NS.len()],
    /// What the idle moment under way spun before its park, nanoseconds, while a sized spin waits for that park to say
    /// how long its gap went on.
    spun: Option<u64>,
}

impl ParkSpin {
    /// What the shard's thread asked, as the loop last took it.
    #[inline]
    pub(crate) fn ask(&self) -> Ask {
        self.ask
    }

    /// The spin in force, nanoseconds: the asked one where fixed, the last window's where sized (0 until a window and a
    /// park's cost say otherwise), none where off.
    #[inline]
    pub(crate) fn spin_ns(&self) -> u64 {
        self.spin_ns
    }

    /// Whether the spin sizes itself from the gaps, so the shard learns what its parks cost.
    #[inline]
    pub(crate) fn sizes(&self) -> bool {
        matches!(self.ask, Ask::Sized { .. })
    }

    /// The gaps the window holds so far: for the loop's tests.
    #[cfg(test)]
    pub(crate) fn window_gaps(&self) -> u64 {
        self.gaps
    }

    /// Takes a new ask: what was measured under the last is let go.
    pub(crate) fn take(&mut self, ask: Ask) {
        *self = ParkSpin {
            ask,
            spin_ns: match ask {
                Ask::Fixed { ns } => ns,
                Ask::Off | Ask::Sized { .. } => 0,
            },
            ..ParkSpin::default()
        };
    }

    /// The idle moment's spin found work after `spun_ns`: its gap. `park_ns`: what a park costs, as measured so far.
    pub(crate) fn hit(&mut self, spun_ns: u64, park_ns: Option<u64>) {
        self.spun = None;
        self.note(spun_ns, park_ns);
    }

    /// The idle moment's spin ran out after `spun_ns` (or there was none): its park says how long the gap went on.
    pub(crate) fn missed(&mut self, spun_ns: u64) {
        self.spun = self.sizes().then_some(spun_ns);
    }

    /// The idle moment's park found work already waiting as it announced itself: its gap ended with the spin.
    pub(crate) fn withdrawn(&mut self, park_ns: Option<u64>) {
        if let Some(spun) = self.spun.take() {
            self.note(spun, park_ns);
        }
    }

    /// The idle moment's park waited, and `woken` says how its wait ended (a sizing shard times its wakes): its gap ran
    /// to the kick's stamp where a kick ended the wait after the park was announced, else to the wait's return.
    pub(crate) fn waited(&mut self, woken: Option<Woken>, park_ns: Option<u64>) {
        let (Some(spun), Some(woken)) = (self.spun.take(), woken) else {
            return;
        };
        let end = woken
            .kicked_ns
            .filter(|kicked| (woken.announced_ns..=woken.returned_ns).contains(kicked))
            .unwrap_or(woken.returned_ns);
        self.note(spun.saturating_add(end.saturating_sub(woken.announced_ns)), park_ns);
    }

    /// Counts one gap of `gap_ns` into a sized spin's window, and sizes the spin anew once the window is full: by the
    /// park's cost `park_ns`, none while no park has been measured.
    fn note(&mut self, gap_ns: u64, park_ns: Option<u64>) {
        let Ask::Sized { cap_ns } = self.ask else {
            return;
        };
        self.gaps = self.gaps.saturating_add(1);
        for (bound, under) in PARK_BOUNDS_NS.iter().zip(self.under.iter_mut()) {
            if *bound > cap_ns {
                break;
            }
            if gap_ns < *bound {
                *under = under.saturating_add(1);
            }
        }
        if self.gaps >= WINDOW {
            self.spin_ns = park_ns.map_or(0, |park_ns| cheapest(&self.under, self.gaps, cap_ns, park_ns));
            self.gaps = 0;
            self.under = [0; PARK_BOUNDS_NS.len()];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The window's cumulative buckets from a list of gaps, as `ParkSpin::note` counts them.
    fn under(gaps: &[u64]) -> [u64; PARK_BOUNDS_NS.len()] {
        let mut under = [0; PARK_BOUNDS_NS.len()];
        for gap in gaps {
            for (bound, count) in PARK_BOUNDS_NS.iter().zip(under.iter_mut()) {
                if gap < bound {
                    *count += 1;
                }
            }
        }
        under
    }

    /// Goal 9, plan 01a1a87b2cec step 4: the cap is one of the park bounds, the first past the cheapest wake the lab
    /// measured (12.6 µs), and the window holds a share within ±5 points at 95 % at its widest.
    #[test]
    fn g9_s4_the_cap_is_a_park_bound_and_the_window_holds_a_share_to_five_points() {
        assert!(PARK_BOUNDS_NS.contains(&CAP_NS));
        assert!(PARK_BOUNDS_NS.iter().filter(|bound| **bound < CAP_NS).all(|bound| *bound < 12_600));
        const { assert!(CAP_NS > 12_600) };
        // In millionths: z · ½ = 1.960 · 0.5 = 0.980, and h, half the convergence width (a tenth, ±5 %), 0.050.
        let spread: u64 = 1_960 * 500;
        let half = crate::machine::stats::CONVERGED_WIDTH_PERMILLE / 2 * 1_000;
        assert_eq!(spread.pow(2).div_ceil(half.pow(2)), WINDOW);
        assert_eq!(WINDOW, 385);
    }

    /// Goal 9, plan 01a1a87b2cec step 4: with no park measured there's nothing to weigh, and gaps longer than the cap
    /// are never worth a spin: each would cost the spin and the park both.
    #[test]
    fn g9_s4_long_gaps_or_free_parks_never_spin() {
        let long = under(&[20_000; 100]);
        assert_eq!(cheapest(&long, 100, CAP_NS, 4_800), 0);
        let short = under(&[500; 100]);
        assert_eq!(cheapest(&short, 100, CAP_NS, 0), 0, "a park that costs nothing is never beaten");
        assert_eq!(cheapest(&short, 0, CAP_NS, 4_800), 0, "no gaps, no spin");
    }

    /// Goal 9, plan 01a1a87b2cec step 4: the spin is the cheapest in CPU on the window's gaps, worked by hand. A park
    /// costs 4.8 µs (a park/wake cycle's CPU on the lab's machine, 4.69 to 4.86 µs: `crate::park_cost`'s notes).
    #[test]
    fn g9_s4_the_spin_is_the_cheapest_in_cpu_on_the_windows_gaps() {
        let park = 4_800;
        // Nine in ten gaps under 1 µs: a 1 µs spin catches them for 1 µs each (9 µs), and the tenth costs 1 + 4.8;
        // 14.8 µs against ten parks' 48 µs. A 2 µs spin costs 9 · 2 + 6.8 = 24.8: the 1 µs spin.
        let mut gaps = vec![500; 9];
        gaps.push(50_000);
        assert_eq!(cheapest(&under(&gaps), 10, CAP_NS, park), 1_000);
        // Gaps of 3 µs: a 4 µs spin costs 4 µs each against 4.8 µs a park.
        assert_eq!(cheapest(&under(&[3_000; 10]), 10, CAP_NS, park), 4_000);
        // Half of them 3 µs, half long: a 4 µs spin costs 5 · 4 + 5 · 8.8 = 64 µs against 48: no spin.
        let mut gaps = vec![3_000; 5];
        gaps.extend([50_000; 5]);
        assert_eq!(cheapest(&under(&gaps), 10, CAP_NS, park), 0);
        // Gaps of 12 µs, under 16: 16 µs each against 4.8 µs a park: no spin, however many.
        assert_eq!(cheapest(&under(&[12_000; 10]), 10, CAP_NS, park), 0);
        // Where a park costs more than the cap (a slow machine's cycle), the 16 µs spin is the cheapest.
        assert_eq!(cheapest(&under(&[12_000; 10]), 10, CAP_NS, 20_000), 16_000);
        // The cap bounds it: asked for no more than 4 µs, 12 µs gaps aren't caught, so no spin.
        assert_eq!(cheapest(&under(&[12_000; 10]), 10, 4_000, 20_000), 0);
    }

    /// Goal 9, plan 01a1a87b2cec step 4: whatever the gaps, the spin chosen never costs more CPU than parking at once
    /// on them, counted as a park costs and a spin spends (each gap at its own length, not its bound's top).
    #[test]
    fn g9_s4_the_chosen_spin_never_costs_more_than_parking() {
        // A deterministic spread of gap lengths and park costs (a linear congruential sequence).
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move || {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
            seed >> 33
        };
        for _ in 0..500 {
            let park = 500 + next() % 30_000;
            let gaps: Vec<u64> = (0..64).map(|_| if next() % 3 == 0 { next() % 40_000 } else { next() % 6_000 }).collect();
            let spin = cheapest(&under(&gaps), 64, CAP_NS, park);
            let spent: u64 = gaps.iter().map(|gap| if *gap < spin { *gap } else { spin + park }).sum();
            assert!(spent <= 64 * park, "a spin of {spin} ns spent {spent} ns where parking cost {}", 64 * park);
        }
    }

    /// Goal 9, plan 01a1a87b2cec step 4: a sized spin starts at none and sizes itself once a window of gaps is in, from
    /// the hits and the parks alike; a gap a park reports runs to the kick's stamp where a kick ended it.
    #[test]
    fn g9_s4_a_sized_spin_sizes_itself_from_a_window_of_gaps() {
        let mut spin = ParkSpin::default();
        spin.take(Ask::Sized { cap_ns: CAP_NS });
        assert_eq!(spin.spin_ns(), 0);
        assert!(spin.sizes());
        // A window of parks each kicked 3 µs after it was announced, though each returned 400 µs after (a lazy wake):
        // their gaps are 3 µs, so the 4 µs spin.
        for i in 0..WINDOW {
            assert_eq!(spin.spin_ns(), 0, "still unsized after {i} gaps");
            spin.missed(0);
            let woken = Woken { announced_ns: 1_000_000, waiting_ns: 1_000_100, returned_ns: 1_400_000, kicked_ns: Some(1_003_000) };
            spin.waited(Some(woken), Some(4_800));
        }
        assert_eq!(spin.spin_ns(), 4_000);
        // The next window: the spin catches each gap after 2.5 µs, under the 4 µs bound and not the 2 µs one. Still the
        // 4 µs spin: 4 µs a gap against 4.8 µs a park.
        for _ in 0..WINDOW {
            spin.hit(2_500, Some(4_800));
        }
        assert_eq!(spin.spin_ns(), 4_000);
        // Then parks that no kick ended, each 400 µs: their gaps run to the wait's return, and the spin goes.
        for _ in 0..WINDOW {
            spin.missed(4_000);
            let woken = Woken { announced_ns: 0, waiting_ns: 100, returned_ns: 400_000, kicked_ns: None };
            spin.waited(Some(woken), Some(4_800));
        }
        assert_eq!(spin.spin_ns(), 0);
    }

    /// Goal 9, plan 01a1a87b2cec step 4: a kick stamped before the park was announced (a sender that read an earlier
    /// park) doesn't end its gap; a park withdrawn ends it with the spin; without a park's cost there's no spin.
    #[test]
    fn g9_s4_stale_stamps_withdrawn_parks_and_unmeasured_costs() {
        let mut spin = ParkSpin::default();
        spin.take(Ask::Sized { cap_ns: CAP_NS });
        for _ in 0..WINDOW {
            spin.missed(0);
            let woken = Woken { announced_ns: 10_000, waiting_ns: 10_100, returned_ns: 900_000, kicked_ns: Some(9_000) };
            spin.waited(Some(woken), Some(4_800));
        }
        assert_eq!(spin.spin_ns(), 0, "a stale stamp's gap ran to the wait's return");
        for _ in 0..WINDOW {
            spin.missed(0);
            spin.withdrawn(Some(4_800));
        }
        assert_eq!(spin.spin_ns(), 1_000, "work already waiting as it parked: a gap of nothing, caught by the least spin");
        for _ in 0..WINDOW {
            spin.missed(0);
            spin.withdrawn(None);
        }
        assert_eq!(spin.spin_ns(), 0, "no park measured, nothing to weigh");
    }

    /// Goal 9, plan 01a1a87b2cec step 4: a fixed spin is what was asked, whatever the gaps; off is none; a new ask lets
    /// go of what the last measured; a park the loop didn't announce after a spin leaves nothing behind.
    #[test]
    fn g9_s4_fixed_and_off_spins_and_a_new_ask() {
        let mut spin = ParkSpin::default();
        assert_eq!(spin.ask(), Ask::Off);
        assert_eq!(spin.spin_ns(), 0);
        spin.take(Ask::Fixed { ns: 16_000 });
        assert!(!spin.sizes());
        for _ in 0..2 * WINDOW {
            spin.missed(16_000);
            spin.waited(Some(Woken { announced_ns: 0, waiting_ns: 1, returned_ns: 900_000, kicked_ns: None }), Some(4_800));
        }
        assert_eq!(spin.spin_ns(), 16_000);
        spin.take(Ask::Sized { cap_ns: CAP_NS });
        assert_eq!(spin.spin_ns(), 0);
        // A spin that ran out, then no park (the shard exited, its driver lost): the next idle moment starts afresh.
        spin.missed(2_000);
        spin.missed(0);
        spin.waited(None, Some(4_800));
        assert_eq!(spin.gaps, 0, "an untimed park says nothing");
        spin.take(Ask::Off);
        assert_eq!(spin.spin_ns(), 0);
        assert!(!spin.sizes());
    }

    /// Goal 9, plan 01a1a87b2cec step 4: only a task on a shard may ask, and its ask is its thread's own.
    #[test]
    fn g9_s4_only_a_shard_may_ask() {
        assert_eq!(ask_current_thread(Ask::Fixed { ns: 1_000 }), Err(RtError::NotOnShardThread));
        assert_eq!(asked(), Ask::Off);
    }
}
