//! What a run of the rule measured on a real node (joint plan step 14, AC-T7.4.1; `docs/design/learned-charge.md`,
//! "Measured in hex-sim, and what ROOT measures"): the figures [`super::sim::Report`] gives of a simulated node, from
//! the windows a fleet's driver reads of a real one, each VM's CPU asked (its trace's), given (its VMM's, as its node
//! reads it) and booked (its holding on its node's capacity answer). Pure: the driver (hex-cli's `charge_fleet`) reads
//! the node and the clock; this reckons, so a hardware run and a simulated one read alike.

use super::sim::jain;
use super::{ChargeError, Tail};

/// One VM's window, each in milli-CPU over the window.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Reading {
    pub vm: u32,
    pub asked: u64,
    pub given: u64,
    pub booked: u64,
}

/// One window of a fleet: the CPU its node's VMs share (the fleet's budget, in milli-CPU) and each VM's reading.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    pub budget: u64,
    pub readings: Vec<Reading>,
}

/// Who plays what in a run: the riser, by VM and the window it rises from, and the process box beside the fleet.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Cast {
    pub riser: Option<(u32, usize)>,
    pub boxed: Option<u32>,
}

/// Nearest-rank quantiles of a run's ratios, per mille.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Ratios {
    pub n: u64,
    pub least: u64,
    pub p1: u64,
    pub p50: u64,
}

/// What a run measured.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Report {
    pub windows: u64,
    /// Windows whose CPU asked passed the budget.
    pub contended: u64,
    /// Of each VM-window of the contended windows that asked and was booked anything, what it was given over
    /// min(asked, booked), per mille: a thousand or more is C4's guarantee kept. Where a window isn't contended, a VM
    /// given less than it asked is given less than its guest could use, which is no share's.
    pub ratios: Ratios,
    /// Of each VM-window of the windows nothing contended that asked anything, what it was given over what it asked,
    /// per mille: the run's own floor, how far a reading falls short of its ask with nothing taking its share (its
    /// node's reading of it, its guest's play), against which the contended windows' ratios are read.
    pub floor: Ratios,
    /// VM-windows of the contended windows given less than min(asked, booked) while the bookings fit the budget: C4's
    /// guarantee broken, its falsifier's own words.
    pub misses: u64,
    /// Jain's index of what VMs were given over what they asked in the contended windows, its least and its mean, per
    /// million.
    pub jain_least_ppm: u64,
    pub jain_mean_ppm: u64,
    /// From the window the riser rises: its neighbours' misses.
    pub neighbours_short: u64,
    /// The process box's misses.
    pub box_short: u64,
    /// The riser's windows from its rise, those it asked past its booking, and what it was given of what it asked,
    /// at the mean, per million.
    pub riser_windows: u64,
    pub riser_over: u64,
    pub riser_given_ppm: u64,
}

/// A part of a whole, per `scale`, rounded down; the whole of it where nothing was asked.
fn part(of: u64, whole: u64, scale: u128) -> u64 {
    u128::from(of)
        .saturating_mul(scale)
        .checked_div(u128::from(whole))
        .map_or(u64::try_from(scale).unwrap_or(u64::MAX), |n| {
            u64::try_from(n).unwrap_or(u64::MAX)
        })
}

/// `ratios`' count, least and nearest-rank quantiles.
fn quantiles(mut ratios: Vec<u64>) -> Ratios {
    ratios.sort_unstable();
    Ratios {
        n: u64::try_from(ratios.len()).unwrap_or(u64::MAX),
        least: ratios.first().copied().unwrap_or(0),
        p1: rank(&ratios, 10),
        p50: rank(&ratios, 500),
    }
}

/// The nearest-rank `q`-quantile, per mille, of `sorted`: its ceil(q n / 1,000)-th smallest.
fn rank(sorted: &[u64], q: usize) -> u64 {
    let at = q
        .saturating_mul(sorted.len())
        .div_ceil(1_000)
        .saturating_sub(1);
    sorted.get(at).copied().unwrap_or(0)
}

/// Reckons `windows` of a run cast as `cast`.
pub fn report(cast: &Cast, windows: &[Window]) -> Report {
    let mut out = Report {
        windows: u64::try_from(windows.len()).unwrap_or(u64::MAX),
        jain_least_ppm: 1_000_000,
        ..Report::default()
    };
    let (mut ratios, mut floor) = (Vec::new(), Vec::new());
    let (mut jain_sum, mut riser_sum) = (0u128, 0u128);
    for (at, window) in windows.iter().enumerate() {
        let asked = window
            .readings
            .iter()
            .fold(0u64, |sum, r| sum.saturating_add(r.asked));
        let booked = window
            .readings
            .iter()
            .fold(0u64, |sum, r| sum.saturating_add(r.booked));
        let (contended, fits) = (asked > window.budget, booked <= window.budget);
        let rising = cast.riser.is_some_and(|(_, from)| at >= from);
        let mut fractions = Vec::new();
        for r in &window.readings {
            let guard = r.asked.min(r.booked);
            let short = contended && fits && r.given < guard;
            if contended && guard > 0 {
                ratios.push(part(r.given, guard, 1_000));
            }
            if !contended && r.asked > 0 {
                floor.push(part(r.given, r.asked, 1_000));
            }
            if short {
                out.misses = out.misses.saturating_add(1);
            }
            let riser = cast.riser.is_some_and(|(vm, _)| vm == r.vm);
            let boxed = cast.boxed == Some(r.vm);
            if short && boxed {
                out.box_short = out.box_short.saturating_add(1);
            }
            if short && rising && !riser && !boxed {
                out.neighbours_short = out.neighbours_short.saturating_add(1);
            }
            if riser && rising {
                out.riser_windows = out.riser_windows.saturating_add(1);
                if r.asked > r.booked {
                    out.riser_over = out.riser_over.saturating_add(1);
                }
                riser_sum = riser_sum.saturating_add(u128::from(part(r.given, r.asked, 1_000_000)));
            }
            if contended && r.asked > 0 {
                fractions.push(part(r.given, r.asked, 1_000_000));
            }
        }
        if contended {
            out.contended = out.contended.saturating_add(1);
            let index = jain(&fractions);
            out.jain_least_ppm = out.jain_least_ppm.min(index);
            jain_sum = jain_sum.saturating_add(u128::from(index));
        }
    }
    out.ratios = quantiles(ratios);
    out.floor = quantiles(floor);
    let mean = |sum: u128, n: u64| {
        u64::try_from(sum.checked_div(u128::from(n)).unwrap_or(0)).unwrap_or(u64::MAX)
    };
    out.jain_mean_ppm = mean(jain_sum, out.contended);
    out.riser_given_ppm = mean(riser_sum, out.riser_windows);
    out
}

/// C1's falsifier on one VM's windows of CPU (in milli-CPU, in order): of the windows after its first SEEDABLE, how
/// many used more than the nearest-rank p99.9 of the SEEDABLE before it, its charge, and how many there were.
pub fn violations(uses: &[u64]) -> Result<(u64, u64), ChargeError> {
    let mut tail = Tail::new()?;
    let (mut past, mut counted) = (0u64, 0u64);
    for use_milli in uses {
        if tail.full()
            && let Some([charge, _, _]) = tail.p999()
        {
            counted = counted.saturating_add(1);
            if *use_milli > charge {
                past = past.saturating_add(1);
            }
        }
        tail.push([*use_milli, 0, 0])?;
    }
    Ok((past, counted))
}

#[cfg(test)]
mod tests;
