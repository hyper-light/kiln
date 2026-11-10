//! W05-R03, W05-R22, W13-R01: GPU placement adapted from Hex 37021c35,
//! hex-sim/src/model.rs (matches/bind) and index.rs (gpu_busy/gpu_idle).
//! Rank requested GPUs by their dominant utilisation/memory share (DRF, NSDI 2011).
//! This reserves node capacity; it does not attach a device to a process or container.

use crate::facts::{Gpu, RunnerFacts};
use super::{needs::Needs, routing::{self, Constraint, Fact}};

const MILLION: u64 = 1_000_000;

/// Per-device pressure, millionths. Missing telemetry remains unknown.
pub fn busy(g: &Gpu) -> Option<u64> {
    let memory = g.memory_used.filter(|_| g.memory > 0).map(|used| {
        u64::try_from(u128::from(used.min(g.memory)) * u128::from(MILLION) / u128::from(g.memory)).unwrap_or(MILLION)
    });
    let compute = g.busy_per_mille.map(|b| u64::from(b.min(1000)) * 1000);
    match (compute, memory) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

fn read(facts: &RunnerFacts, g: &Gpu, name: &str) -> Fact {
    match name.to_ascii_lowercase().as_str() {
        "gpu.model" => Fact::List(vec![g.model.clone(), g.model.split_whitespace().collect::<Vec<_>>().join("-")]),
        "gpu.vendor" => Fact::List(vec![g.vendor.clone()]),
        "gpu.memory" => Fact::Num(g.memory as f64),
        "gpu.memory.free" => g.memory_used.map_or(Fact::Missing, |used| Fact::Num(g.memory.saturating_sub(used) as f64)),
        "gpu.utilization" => g.busy_per_mille.map_or(Fact::Missing, |b| Fact::Num(f64::from(b.min(1000)))),
        _ => routing::fact(facts, name),
    }
}

/// The same cards must meet all model/vendor/memory predicates, including Boolean expressions.
/// Counting a model on one card and memory on another must never satisfy a multi-GPU request.
pub fn matches(needs: &Needs, facts: &RunnerFacts, labels: &[String]) -> bool {
    let Ok(cs) = needs.constraints() else { return false };
    if needs.request.gpu == 0 {
        return cs.iter().all(|(_, c)| c.eval(facts, labels));
    }
    matching(&cs, facts, labels).count() >= usize::try_from(needs.request.gpu).unwrap_or(usize::MAX)
}

fn matching<'a>(cs: &'a [(String, Constraint)], facts: &'a RunnerFacts, labels: &'a [String]) -> impl Iterator<Item = &'a Gpu> {
    facts.gpus.iter().filter(move |g| cs.iter().all(|(_, c)| c.eval_with(&|name| read(facts, g, name), labels)))
}

/// Best requested cards' total idle share. Reservations are aggregate, so conservatively remove
/// already-held cards from the matching pool. Unknown load uses the measured fleet mean (Hex's
/// finish-model prior rule), or zero information when the fleet has no observations.
pub fn rank(needs: &Needs, facts: &RunnerFacts, labels: &[String], free: u64, prior: u64) -> Option<u64> {
    let n = usize::try_from(needs.request.gpu).ok()?;
    if n == 0 { return Some(0); }
    let cs = needs.constraints().ok()?;
    let held = facts.gpus.len().saturating_sub(usize::try_from(free).unwrap_or(usize::MAX));
    let mut scores: Vec<u64> = matching(&cs, facts, labels).map(|g| MILLION.saturating_sub(busy(g).unwrap_or(prior))).collect();
    if scores.len().saturating_sub(held) < n { return None; }
    scores.sort_unstable_by(|a, b| b.cmp(a));
    Some(scores.into_iter().skip(held).take(n).fold(0, u64::saturating_add))
}

/// Measured mean for missing counters; no fabricated utilisation on a cold fleet.
pub fn prior<'a>(gpus: impl Iterator<Item = &'a Gpu>) -> u64 {
    let (sum, n) = gpus.filter_map(busy).fold((0u128, 0u128), |(sum, n), b| (sum.saturating_add(u128::from(b)), n.saturating_add(1)));
    u64::try_from(sum.checked_div(n).unwrap_or(0)).unwrap_or(MILLION)
}

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::needs;
    const GIB: u64 = 1 << 30;
    fn card(model: &str, memory: u64, used: Option<u64>, busy: Option<u16>) -> Gpu {
        Gpu { vendor: "nvidia".into(), model: model.into(), memory: memory * GIB, memory_used: used.map(|n| n * GIB), busy_per_mille: busy, ..Gpu::default() }
    }
    #[test]
    fn w05_r22_gpu_predicates_must_hold_on_each_requested_card() {
        let mut f = RunnerFacts { gpus: vec![card("A100", 8, Some(0), Some(0)), card("H100", 80, Some(0), Some(0))], ..RunnerFacts::default() };
        let n = needs::parse("gpus=1,gpu.model==A100,gpu.memory>=16Gi").unwrap();
        assert!(!matches(&n, &f, &[])); // Aggregate model + maximum memory was a false fit.
        f.gpus.push(card("A100", 80, Some(64), Some(100)));
        assert!(matches(&n, &f, &[]));
        let two = needs::parse("gpus=2,gpu.model==A100,gpu.memory.free>=16Gi").unwrap();
        assert!(!matches(&two, &f, &[])); // One qualifying card cannot satisfy two GPUs.
        let mut one = two.clone(); one.request.gpu = 1;
        assert!(matches(&one, &f, &[]));
    }
    #[test]
    fn w05_r23_gpu_rank_uses_dominant_pressure_and_matching_cards_only() {
        let n = needs::parse("gpus=1,gpu.model==A100").unwrap();
        let f = RunnerFacts { gpus: vec![card("A100", 80, Some(72), Some(10)), card("H100", 80, Some(0), Some(0))], ..RunnerFacts::default() };
        assert_eq!(rank(&n, &f, &[], 2, 0), Some(100_000));
        assert_eq!(rank(&n, &f, &[], 1, 0), None); // Conservative aggregate reservation.
        assert_eq!(busy(&card("A100", 80, Some(8), Some(900))), Some(900_000));
        assert_eq!(busy(&card("A100", 80, Some(100), Some(2000))), Some(1_000_000));
    }
    #[test]
    fn w13_r01_missing_gpu_measurements_are_not_fabricated_idle() {
        let known = card("A100", 80, Some(40), None);
        let unknown = card("A100", 80, None, None);
        assert_eq!(busy(&unknown), None);
        let p = prior([&known, &unknown].into_iter());
        assert_eq!(p, 500_000);
        let f = RunnerFacts { gpus: vec![unknown], ..RunnerFacts::default() };
        let n = needs::parse("gpus=1").unwrap();
        assert_eq!(rank(&n, &f, &[], 1, p), Some(500_000));
        let n = needs::parse("gpus=1,gpu.memory.free>=1Gi").unwrap();
        assert!(!matches(&n, &f, &[]));
    }
    #[test]
    fn w05_r22_gpu_boolean_constraints_keep_host_facts_and_count() {
        let f = RunnerFacts { os: "linux".into(), gpus: vec![card("NVIDIA A100", 80, Some(0), Some(0)), card("H100", 80, Some(0), Some(0))], ..RunnerFacts::default() };
        let n = Needs { request: super::super::state::Resources { gpu: 1, ..Default::default() }, requires: vec!["os == linux && gpu.count == 2 && (gpu.model == nvidia-a100 || gpu.model == H100)".into()], ..Needs::default() };
        assert!(matches(&n, &f, &[]));
        let mut bad = n; bad.requires.push("gpu.vendor == amd".into());
        assert!(!matches(&bad, &f, &[]));
    }
    #[test]
    fn w13_r01_gpu_facts_old_and_new_json_are_compatible() {
        let old: Gpu = serde_json::from_str(r#"{"vendor":"nvidia","model":"A100","memory":80}"#).unwrap();
        assert_eq!(busy(&old), None);
        let json = serde_json::to_value(&old).unwrap();
        assert!(json.get("memory_used").is_none());
        assert!(json.get("busy_per_mille").is_none());
        assert_eq!(serde_json::from_value::<Gpu>(json).unwrap(), old);
    }
}
