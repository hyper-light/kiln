//! A deployment experiment's draw (kiln plan 01a1dd2ea7a777a28970a43fc5f766d0 P7.1, an agent definition's experiment,
//! `kiln::agentdefs::experiments`): which variant each new session gets, by the variants' weights. The draw for the
//! `n`-th session is a point of [0, 1) from the experiment's seed and `n` (splitmix64), laid on the variants' weights in
//! their order: reproducible, and each variant's share is its weight's.

/// A point of [0, 1) from `seed` and `n` (splitmix64).
fn unit(seed: u64, n: u64) -> f64 {
    let mut z = seed ^ n.wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    ((z ^ (z >> 31)) >> 11) as f64 / (1u64 << 53) as f64
}

/// The variant the `n`-th session of a split gets, by `weights` (each above 0) in their order; none without one.
pub fn draw(weights: &[f64], seed: u64, n: u64) -> Option<usize> {
    let total: f64 = weights.iter().sum();
    if weights.is_empty() || weights.iter().any(|w| !(w.is_finite() && *w > 0.0)) {
        return None;
    }
    let at = unit(seed, n) * total;
    let mut sum = 0.0;
    for (k, w) in weights.iter().enumerate() {
        sum += w;
        if at < sum {
            return Some(k);
        }
    }
    weights.len().checked_sub(1)
}

/// The variant the `n`-th session of a split gets by `parts`, each variant's whole share in their order (a definition's
/// experiment's percentages, kiln plan 01a1cd8e862e7169a074db3d5fa33f3d P7.1): [`draw`] over those above 0, so a
/// variant at 0 gets none and the others draw as they would without it. None when none is above 0.
pub fn draw_parts(parts: &[u32], seed: u64, n: u64) -> Option<usize> {
    let some: Vec<(usize, f64)> = parts.iter().enumerate().filter(|(_, p)| **p > 0).map(|(k, p)| (k, f64::from(*p))).collect();
    let weights: Vec<f64> = some.iter().map(|(_, w)| *w).collect();
    some.get(draw(&weights, seed, n)?).map(|(k, _)| *k)
}

/// `parts` scaled in proportion to add up to `total`, each whole: each its proportion rounded down, then one more to
/// each of those with the largest remainders (the earliest first among equals) until they add up (the largest
/// remainder method). A definition's experiment's weights once a removed variant's share goes back to the others
/// (P7.1); one at 0 stays at 0. None when they add up to nothing.
pub fn rescale(parts: &[u32], total: u32) -> Option<Vec<u32>> {
    let sum: u64 = parts.iter().map(|p| u64::from(*p)).sum();
    if sum == 0 {
        return None;
    }
    let total = u64::from(total);
    let mut out: Vec<u32> = Vec::with_capacity(parts.len());
    let mut remainders: Vec<(u64, usize)> = Vec::with_capacity(parts.len());
    for (k, p) in parts.iter().enumerate() {
        let scaled = u64::from(*p) * total;
        out.push(u32::try_from(scaled / sum).ok()?);
        remainders.push((scaled % sum, k));
    }
    let given: u64 = out.iter().map(|w| u64::from(*w)).sum();
    let left = usize::try_from(total.saturating_sub(given)).ok()?;
    remainders.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    for (_, k) in remainders.into_iter().take(left) {
        if let Some(w) = out.get_mut(k) {
            *w += 1;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// P7.1 ("with weights 70/30 over N starts, the assignment proportions fall within their binomial interval"):
    /// 2,000 draws at 70/30 give the first variant a share within 3.29 standard errors of 0.7 (99.9 %), for each of
    /// several seeds; weights that aren't all above 0 draw nothing.
    #[test]
    fn p7_1_draws_fall_within_their_binomial_interval() {
        let n = 2000u64;
        for seed in [1u64, 42, 0xDEAD_BEEF] {
            let first = (0..n).filter(|i| draw(&[70.0, 30.0], seed, *i) == Some(0)).count() as f64;
            let share = first / n as f64;
            let se = (0.7f64 * 0.3 / n as f64).sqrt();
            assert!((share - 0.7).abs() <= 3.29 * se, "seed {seed}: {share}");
        }
        assert_eq!(draw(&[70.0, 0.0], 1, 0), None);
        assert_eq!(draw(&[], 1, 0), None);
        assert_eq!((0..100).map(|i| draw(&[1.0, 1.0, 1.0], 9, i)).collect::<Vec<_>>(), (0..100).map(|i| draw(&[1.0, 1.0, 1.0], 9, i)).collect::<Vec<_>>(), "the same experiment draws the same again");
    }

    /// P7.1 (a definition's experiment, "assignment by weight, deterministic"): whole-percent shares draw as their
    /// weights would, a variant at 0 gets no session, and adding one at 0 changes no other draw; 2,000 draws at
    /// 20/0/80 give the last a share within 3.29 standard errors of 0.8 (99.9 %). Shares none of which is above 0
    /// draw nothing.
    #[test]
    fn p7_1_whole_parts_draw_by_weight_and_a_variant_at_zero_gets_none() {
        let n = 2000u64;
        for seed in [7u64, 99, 0xC0FF_EE00] {
            let drawn: Vec<Option<usize>> = (0..n).map(|i| draw_parts(&[20, 0, 80], seed, i)).collect();
            assert!(drawn.iter().all(|d| matches!(d, Some(0 | 2))), "seed {seed}: a variant at 0 was drawn");
            let without: Vec<Option<usize>> = (0..n).map(|i| draw(&[20.0, 80.0], seed, i).map(|k| if k == 0 { 0 } else { 2 })).collect();
            assert_eq!(drawn, without, "seed {seed}: one at 0 changes no draw");
            let last = drawn.iter().filter(|d| **d == Some(2)).count() as f64 / n as f64;
            let se = (0.8f64 * 0.2 / n as f64).sqrt();
            assert!((last - 0.8).abs() <= 3.29 * se, "seed {seed}: {last}");
        }
        assert_eq!(draw_parts(&[0, 0], 1, 0), None);
        assert_eq!(draw_parts(&[], 1, 0), None);
        assert_eq!(draw_parts(&[0, 100], 3, 5), Some(1));
    }

    /// P7.1 (a removed variant's share goes back to the others, in proportion): the rest add up to the whole exactly,
    /// the largest remainders taking what rounding down left, the earliest first among equals; one at 0 stays at 0;
    /// none to scale is none.
    #[test]
    fn p7_1_a_share_given_back_keeps_the_others_in_proportion_and_whole() {
        assert_eq!(rescale(&[30, 30, 30], 100), Some(vec![34, 33, 33]));
        assert_eq!(rescale(&[25, 25], 100), Some(vec![50, 50]));
        assert_eq!(rescale(&[10, 0, 30], 100), Some(vec![25, 0, 75]));
        assert_eq!(rescale(&[1, 2], 100), Some(vec![33, 67]));
        assert_eq!(rescale(&[0, 0], 100), None);
        assert_eq!(rescale(&[], 100), None);
        for parts in [vec![7u32, 11, 13, 17], vec![1, 1, 1, 1, 1, 1, 1], vec![99, 1], vec![3, 0, 0, 5]] {
            let out = rescale(&parts, 100).unwrap_or_default();
            assert_eq!(out.iter().sum::<u32>(), 100, "{parts:?} → {out:?}");
            assert!(parts.iter().zip(&out).all(|(p, o)| *p != 0 || *o == 0), "{parts:?} → {out:?}");
        }
    }
}
