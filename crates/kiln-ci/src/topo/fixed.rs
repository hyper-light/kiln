//! The topology's arithmetic: integers only, every operation saturating, so the same inputs give
//! the same answers bit for bit on x86-64 and arm64. Rust defines its integer operations exactly;
//! `f64`'s `exp` and `ln` come from each platform's libm and may differ in the last bit, which is
//! why nothing here uses them.
//!
//! Fractions are Q32: `ONE` (2^32) is 1.0, so a relative error of 5 % is `ONE / 20`.

/// 1.0 in Q32.
pub const ONE: u64 = 1 << 32;
const SHIFT: u32 = 32;

/// `a × b`, Q32 by Q32 (or an integer by a Q32 fraction).
pub fn mul(a: u64, b: u64) -> u64 {
    sat((u128::from(a) * u128::from(b)) >> SHIFT)
}

/// `a / b` as a Q32 fraction; a zero `b` gives the largest value.
pub fn ratio(a: u64, b: u64) -> u64 {
    if b == 0 {
        return u64::MAX;
    }
    sat((u128::from(a) << SHIFT) / u128::from(b))
}

/// A signed integer times a Q32 fraction, rounded toward negative infinity.
pub fn mul_i(a: i64, q: u64) -> i64 {
    let p = (i128::from(a) * i128::from(q)) >> SHIFT;
    i64::try_from(p).unwrap_or(if p < 0 { i64::MIN } else { i64::MAX })
}

/// A signed integer times a signed Q32 fraction, rounded toward negative infinity.
pub fn mul_s(a: i64, q: i64) -> i64 {
    let p = (i128::from(a) * i128::from(q)) >> SHIFT;
    i64::try_from(p).unwrap_or(if p < 0 { i64::MIN } else { i64::MAX })
}

/// √a for a Q32 `a`, in Q32.
pub fn sqrt(a: u64) -> u64 {
    sat((u128::from(a) << SHIFT).isqrt())
}

/// A `u128` held to `u64`.
pub fn sat(v: u128) -> u64 {
    u64::try_from(v).unwrap_or(u64::MAX)
}

/// ln 2 in Q32, rounded to nearest: 0.693147180559945309… × 2^32 = 2977044471.82.
const LN2: u64 = 2_977_044_472;

/// e^(−x) for a Q32 `x ≥ 0`, in Q32: `x = k·ln 2 + r` with `0 ≤ r < ln 2`, so `e^(−x) =
/// 2^(−k)·e^(−r)`, and `e^(−r)` from its Taylor series, whose terms past the 13th are below
/// `ln 2^14 / 14! < 2^−37`, under Q32's resolution.
pub fn exp_neg(x: u64) -> u64 {
    let k = x / LN2;
    if k >= u64::from(SHIFT) {
        return 0;
    }
    let r = x - k * LN2;
    // Σ (−r)^n / n!, n = 0..=13, the alternating terms kept as magnitudes.
    let (mut sum, mut term): (i128, u128) = (i128::from(ONE), u128::from(ONE));
    for n in 1..=13u128 {
        term = ((term * u128::from(r)) >> SHIFT) / n;
        let t = i128::try_from(term).unwrap_or(i128::MAX);
        sum = if n % 2 == 1 { sum - t } else { sum + t };
    }
    let e = u64::try_from(sum.max(0)).unwrap_or(0);
    e >> k
}

/// 1 − e^(−x) for a Q32 `x ≥ 0`: the chance an event of a Poisson process happens within a span
/// the process expects `x` of.
pub fn one_minus_exp_neg(x: u64) -> u64 {
    ONE.saturating_sub(exp_neg(x))
}

/// SplitMix64 (Steele, Lea and Flood, "Fast Splittable Pseudorandom Number Generators", OOPSLA
/// 2014): the next number of the stream `state`, the same on every machine.
pub fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// FNV-1a, 64 bits (Fowler, Noll and Vo; IETF draft-eastlake-fnv): a name's number, the same on
/// every machine, for the streams nodes draw directions from.
pub fn id_of(name: &str) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for b in name.bytes() {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01B3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// e^(−x) against its known values to within Q32's resolution and a few units.
    #[test]
    fn lb08_exp_is_exact_to_q32() {
        let near = |got: u64, want: f64| {
            #[allow(clippy::cast_precision_loss, clippy::float_arithmetic)]
            let g = got as f64 / ONE as f64;
            #[allow(clippy::float_arithmetic)]
            let d = (g - want).abs();
            assert!(d < 1e-8, "got {g}, want {want}");
        };
        near(exp_neg(0), 1.0);
        near(exp_neg(ONE), 0.367_879_441_171_442_3);
        near(exp_neg(ONE / 2), 0.606_530_659_712_633_4);
        near(exp_neg(3 * ONE), 0.049_787_068_367_863_94);
        near(exp_neg(10 * ONE), 4.539_992_976_248_485e-5);
        assert_eq!(exp_neg(40 * ONE), 0);
        near(one_minus_exp_neg(ONE / 1000), 0.000_999_500_166_625_008_3);
        assert_eq!(sqrt(4 * ONE), 2 * ONE);
        assert_eq!(ratio(1, 4), ONE / 4);
        assert_eq!(mul_i(-100, ONE / 4), -25);
    }
}
