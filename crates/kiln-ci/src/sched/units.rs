//! The engine's units (docs/needs.md, E1): what `sched` decides with is integers, milliseconds,
//! millionths of a share and thousandths of a weight or factor, so a decision comes out the same
//! on every machine and in every replay. Floats come in from outside (what runners report, what
//! people write in YAML and config, history's seconds) and go out in explanations people read;
//! they're converted here and nowhere else in `sched`, where clippy's `float_arithmetic` is
//! denied.

/// A duration, a cost or a wait, in milliseconds.
pub type Ms = u64;

/// A share from none to all, in millionths.
pub type Ppm = u32;

/// All of a share, in millionths: one.
pub const WHOLE: Ppm = 1_000_000;

/// Seconds as they come from outside, in milliseconds to the nearest: none for one that isn't a
/// finite number; below zero is zero.
#[allow(clippy::float_arithmetic, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn ms_of_secs(secs: f64) -> Option<Ms> {
    secs.is_finite().then(|| (secs * 1000.0).round().clamp(0.0, u64::MAX as f64) as u64)
}

/// A share (0 to 1) as it comes from outside, in millionths, held to 0..1; NaN is none.
#[allow(clippy::float_arithmetic, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn ppm_of(share: f64) -> Ppm {
    if share.is_nan() {
        return 0;
    }
    (share.clamp(0.0, 1.0) * f64::from(WHOLE)).round() as u32
}

/// A weight as it's written (`1`, `2.5`, `-1`), in thousandths: 0 for one that isn't a finite
/// number.
#[allow(clippy::float_arithmetic, clippy::cast_possible_truncation, clippy::cast_precision_loss)]
pub fn milli_of(x: f64) -> i64 {
    if !x.is_finite() {
        return 0;
    }
    (x * 1000.0).round().clamp(i64::MIN as f64, i64::MAX as f64) as i64
}

/// A weight or share of fairness as config gives it (`1`, `3`, `0.5`), in thousandths: 0 for
/// one that isn't a finite number above zero, which counts as no weight given.
#[allow(clippy::float_arithmetic, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn thousandths_of(x: f64) -> u32 {
    if !x.is_finite() || x <= 0.0 {
        return 0;
    }
    (x * 1000.0).round().clamp(0.0, f64::from(u32::MAX)) as u32
}

/// A number as it comes from outside (a fact a runner reports, a value people wrote), in
/// millionths of it: enough to compare and scale it in integers. None for one that isn't finite.
#[allow(clippy::float_arithmetic, clippy::cast_possible_truncation)]
pub fn micro_of(x: f64) -> Option<i128> {
    x.is_finite().then(|| (x * 1e6).round() as i128)
}

/// Bytes a second as a runner reports them: none for one that isn't a finite number above zero.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn bytes_per_s(b: f64) -> Option<u64> {
    (b.is_finite() && b > 0.0).then(|| b.round().clamp(1.0, u64::MAX as f64) as u64)
}

/// A percentage (0 to 100) as it comes from outside, in hundredths of a percent, held to 0..100:
/// none for one that isn't a finite number.
#[allow(clippy::float_arithmetic, clippy::cast_possible_truncation, clippy::cast_sign_loss)]
pub fn hundredths_of_percent(p: f64) -> Option<u32> {
    p.is_finite().then(|| (p.clamp(0.0, 100.0) * 100.0).round() as u32)
}

/// Milliseconds as seconds, for explanations people read.
#[allow(clippy::float_arithmetic, clippy::cast_precision_loss)]
pub fn secs(ms: Ms) -> f64 {
    ms as f64 / 1000.0
}

/// Milliseconds as seconds, for explanations, `Ms::MAX` (never) as infinity.
pub fn secs_or_never(ms: Ms) -> f64 {
    if ms == Ms::MAX { f64::INFINITY } else { secs(ms) }
}

/// Millionths as a share, for explanations.
#[allow(clippy::float_arithmetic)]
pub fn share(ppm: Ppm) -> f64 {
    f64::from(ppm) / f64::from(WHOLE)
}

/// A preference score (weights in thousandths × shares in millionths), as the weights it's made of
/// are written, for explanations.
#[allow(clippy::float_arithmetic, clippy::cast_precision_loss)]
pub fn score(points: i64) -> f64 {
    points as f64 / 1e9
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each conversion at the engine's edge: rounding to the nearest, what isn't a number refused or
    /// none, and what goes out reading as what came in.
    #[test]
    fn e1_2_conversions_at_the_edge() {
        assert_eq!(ms_of_secs(1.5), Some(1500));
        assert_eq!(ms_of_secs(0.0004), Some(0));
        assert_eq!(ms_of_secs(-5.0), Some(0), "below zero is zero");
        assert_eq!(ms_of_secs(f64::NAN), None);
        assert_eq!(ms_of_secs(f64::INFINITY), None);
        assert_eq!((ppm_of(0.5), ppm_of(1.5), ppm_of(-1.0), ppm_of(f64::NAN)), (500_000, WHOLE, 0, 0));
        assert_eq!((milli_of(2.5), milli_of(-1.0), milli_of(f64::NAN)), (2500, -1000, 0));
        assert_eq!((thousandths_of(3.0), thousandths_of(0.0), thousandths_of(-2.0)), (3000, 0, 0));
        assert_eq!((micro_of(0.25), micro_of(f64::INFINITY)), (Some(250_000), None));
        assert_eq!((bytes_per_s(1.0e8), bytes_per_s(0.0), bytes_per_s(f64::NAN)), (Some(100_000_000), None, None));
        assert_eq!((hundredths_of_percent(39.5), hundredths_of_percent(140.0), hundredths_of_percent(f64::NAN)), (Some(3950), Some(10_000), None));
        assert_eq!((secs(10_240), share(375_000), score(1_500_000_000)), (10.24, 0.375, 1.5));
        assert!(secs_or_never(Ms::MAX).is_infinite());
    }
}
