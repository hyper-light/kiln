//! When the replicated owner compacts its raft log (replication.md, "Decision (lane constants): the
//! compaction's expansion learned where it runs"; CT.1). Ongaro's thesis §5.1.2 makes the log due
//! once it holds more than its last image times an expansion factor, which it leaves to the owner
//! (hyper-durable's [`Compaction`]). The owner learns the factor from what it measures where it
//! runs: the nanoseconds a byte of image takes to make, and those a byte of log takes to replay
//! (to apply, as a restart or a member behind does). The log is due once replaying it would cost
//! more than making a fresh image of the state it leads to: the break-even of ski rental (Karlin,
//! Manasse, McGeoch and Owicki, "Competitive Randomized Algorithms for Non-Uniform Problems",
//! SODA 1990: buying once the rent paid reaches the price is within twice the best in hindsight),
//! the log's bytes the rent and the image the price. So `expansion = image cost / replay cost`,
//! raised to the whole factor the rule takes: never due before the break-even.
//!
//! A pure core: the owner tells it the bytes and nanoseconds each image and each drive that applied
//! entries took, on its own clock, and asks it for the rule. Each cost is a decaying ratio of sums,
//! nanoseconds over bytes, each sum kept at seven eighths of itself before a sample is added: RFC
//! 6298 §2's smoothing gain of 1/8 (Jacobson, "Congestion Avoidance and Control", SIGCOMM 1988),
//! weighted by the bytes each sample measured, so the costs follow the host and its load as they
//! move. A sample that took no time on the clock is below its resolution and tells nothing.
//!
//! Before both costs are measured, the factor is one: with nothing measured, a byte of log costs a
//! byte of image, whose break-even is the log at its image's own size.

use hyper_durable::Compaction;

/// One cost: the nanoseconds and bytes measured, each decayed by the gain at every sample.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Cost {
    nanos: u64,
    bytes: u64,
}

impl Cost {
    fn add(&mut self, bytes: u64, nanos: u64) {
        if bytes == 0 || nanos == 0 {
            return;
        }
        // RFC 6298 §2's gain: the old sums keep seven eighths, a shift of three.
        let decay = |sum: u64| sum.saturating_sub(sum.checked_shr(3).unwrap_or(0));
        self.nanos = decay(self.nanos).saturating_add(nanos);
        self.bytes = decay(self.bytes).saturating_add(bytes);
    }
    fn measured(self) -> bool {
        self.nanos > 0 && self.bytes > 0
    }
}

/// What the owner measured of its images and its log's replay, and the compaction rule from them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) struct Costs {
    image: Cost,
    replay: Cost,
}

impl Costs {
    /// An image of `bytes` took `nanos` to make.
    pub(super) fn imaged(&mut self, bytes: u64, nanos: u64) {
        self.image.add(bytes, nanos);
    }
    /// A drive applied `bytes` of entries in `nanos`.
    pub(super) fn replayed(&mut self, bytes: u64, nanos: u64) {
        self.replay.add(bytes, nanos);
    }
    /// The expansion factor in force: the image's cost a byte over the replay's, raised to a whole
    /// factor; one before both are measured.
    pub(super) fn expansion(&self) -> u64 {
        if !self.image.measured() || !self.replay.measured() {
            return 1;
        }
        // (image nanos / image bytes) / (replay nanos / replay bytes), in u128: no product of two
        // u64 overflows it.
        let over = u128::from(self.image.nanos).saturating_mul(u128::from(self.replay.bytes));
        let under = u128::from(self.image.bytes).saturating_mul(u128::from(self.replay.nanos));
        let ratio = over.div_ceil(under.max(1));
        u64::try_from(ratio).unwrap_or(u64::MAX).max(1)
    }
    /// The rule hyper-durable's `Replica::compaction_due` weighs the log by.
    pub(super) fn rule(&self) -> Compaction {
        Compaction {
            expansion: self.expansion(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Costs;

    /// T-CT.1.1: nothing measured, or only one of the two costs, or samples that took no time on
    /// the driven clock — passes when the factor is one throughout.
    #[test]
    fn compaction_the_factor_is_one_until_both_costs_are_measured() {
        let mut costs = Costs::default();
        assert_eq!(costs.expansion(), 1);
        costs.imaged(1 << 20, 0);
        costs.replayed(1 << 20, 0);
        assert_eq!(
            costs.expansion(),
            1,
            "a sample below the clock's resolution"
        );
        costs.imaged(1 << 20, 4_000_000);
        assert_eq!(costs.expansion(), 1, "the replay not yet measured");
        let mut costs = Costs::default();
        costs.replayed(1 << 20, 1_000_000);
        assert_eq!(costs.expansion(), 1, "the image not yet measured");
    }

    /// T-CT.1.2: images four times dearer a byte than replay, then a third as dear, then dearer
    /// again as the host slows its images — passes when the factor is four, then one (never due
    /// before the break-even), then follows the images' cost up.
    #[test]
    fn compaction_the_factor_is_the_image_cost_over_the_replay_cost() {
        let mut costs = Costs::default();
        // 1 MiB of image in 4 ms, 1 MiB of log replayed in 1 ms.
        costs.imaged(1 << 20, 4_000_000);
        costs.replayed(1 << 20, 1_000_000);
        assert_eq!(costs.expansion(), 4);
        assert_eq!(costs.rule().expansion, 4);
        let mut cheap = Costs::default();
        cheap.imaged(3 << 20, 1_000_000);
        cheap.replayed(1 << 20, 1_000_000);
        assert_eq!(
            cheap.expansion(),
            1,
            "a third rounds up to the whole factor"
        );
        // The images grow dearer: each sample moves the factor up, by RFC 6298's gain.
        let before = costs.expansion();
        for _ in 0..64 {
            costs.imaged(1 << 20, 16_000_000);
        }
        let after = costs.expansion();
        assert!(after > before, "{before} -> {after}");
        assert_eq!(after, 16, "the old samples decayed away");
    }

    /// T-CT.1.3: a large sample beside a small one — passes when each weighs by its bytes.
    #[test]
    fn compaction_samples_weigh_by_their_bytes() {
        let mut costs = Costs::default();
        costs.replayed(1 << 20, 1_000_000);
        // A tiny drive that took long (a step's other work) moves the cost little.
        costs.replayed(1, 1_000_000);
        costs.imaged(1 << 20, 8_000_000);
        let factor = costs.expansion();
        assert!((3..=8).contains(&factor), "{factor}");
    }
}
