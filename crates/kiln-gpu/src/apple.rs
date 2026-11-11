//! Apple's native adapter and the integer residency calculation it uses (`AC-T7.3.1`).
//! IOReport names idle states IDLE/OFF/DOWN; active / total residency is its busy fraction.
//! Adapted from lablup/all-smi fe479ae6, macos_native/ioreport.rs (Apache-2.0).
//! Source, memory semantics and unverified native effects: hex-macos-gpu/README.md.

#[cfg(target_os = "macos")]
pub(crate) mod sys;

use crate::Unread;

#[derive(Default)]
struct Residency {
    active: u128,
    total: u128,
}

impl Residency {
    fn add(&mut self, idle: bool, ticks: i64) -> Result<(), Unread> {
        let ticks = u128::try_from(ticks).map_err(|_| Unread::Malformed)?;
        self.total = self.total.checked_add(ticks).ok_or(Unread::Malformed)?;
        if !idle {
            self.active = self.active.checked_add(ticks).ok_or(Unread::Malformed)?;
        }
        Ok(())
    }

    fn percent(self) -> Result<u8, Unread> {
        if self.total == 0 {
            return Err(Unread::NoSample);
        }
        // Percent's 100 is a unit conversion, not a sampling/tuning constant. Floor to the public
        // integral percent; i32 state count * i64 residency fits u128, but still check the product.
        let percent = self
            .active
            .checked_mul(100)
            .and_then(|value| value.checked_div(self.total))
            .ok_or(Unread::Malformed)?;
        u8::try_from(percent)
            .ok()
            .filter(|value| *value <= 100)
            .ok_or(Unread::Malformed)
    }
}

/// GPUPH is a package counter. Attribute it only to a sole enumerated Metal device; never repeat
/// the package value across several identities or use an unreadable sibling's value.
fn package_counter(devices: usize) -> Result<(), Unread> {
    match devices {
        0 => Err(Unread::Gone),
        1 => Ok(()),
        _ => Err(Unread::Ambiguous),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-T7.3.1: do vary the caller's observation window; expect the same actual busy fraction,
    /// with no imposed minimum interval, and a genuine idle window distinguished from no sample.
    #[test]
    fn residency_uses_observed_ticks_and_distinguishes_idle_from_no_sample() {
        for scale in [1, 17, 1_000_000] {
            let mut residency = Residency::default();
            residency.add(true, 3 * scale).unwrap();
            residency.add(false, scale).unwrap();
            assert_eq!(residency.percent(), Ok(25));
        }
        let mut idle = Residency::default();
        idle.add(true, 19).unwrap();
        idle.add(false, 0).unwrap();
        assert_eq!(idle.percent(), Ok(0));
        assert_eq!(Residency::default().percent(), Err(Unread::NoSample));
    }

    /// AC-T7.3.1: do feed native signed counters and their integer limits; expect exact flooring
    /// without wrap, and a negative/reset delta refused rather than presented as idle.
    #[test]
    fn residency_refuses_negative_values_and_keeps_wide_sums() {
        let mut bad = Residency::default();
        assert_eq!(bad.add(false, -1), Err(Unread::Malformed));
        let mut large = Residency::default();
        for idle in [true, false, false] {
            large.add(idle, i64::MAX).unwrap();
        }
        assert_eq!(large.percent(), Ok(66));
        let mut busy = Residency::default();
        busy.add(false, i64::MAX).unwrap();
        assert_eq!(busy.percent(), Ok(100));
    }

    /// AC-T7.3.1: do enumerate multiple native devices; expect no guessed per-device attribution
    /// of the sole package-wide GPUPH channel. A sole native identity remains usable.
    #[test]
    fn a_package_counter_is_never_copied_to_multiple_devices() {
        assert_eq!(package_counter(0), Err(Unread::Gone));
        assert_eq!(package_counter(1), Ok(()));
        assert_eq!(package_counter(2), Err(Unread::Ambiguous));
        assert_eq!(package_counter(usize::MAX), Err(Unread::Ambiguous));
    }
}
