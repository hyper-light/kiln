//! W05-R03, W05-R22, W13-R01: GPU placement adapted from Hex 37021c35,
//! hex-sim/src/model.rs (matches/bind) and index.rs (gpu_busy/gpu_idle).
//! Fresh-reading quality follows Hex b5b67b38: unknown is never idle or unused memory.
//! Rank requested GPUs by their dominant utilisation/memory share (DRF, NSDI 2011).
//! This reserves node capacity; it does not attach a device to a process or container.

use crate::facts::{Gpu, RunnerFacts};
use super::{needs::Needs, routing::{self, Constraint, Fact}};

const MILLION: u64 = 1_000_000;

/// Per-device pressure, millionths. Missing telemetry remains unknown.
pub fn busy(g: &Gpu) -> Option<u64> {
    let (compute, used) = observed(g)?;
    let memory = (u128::from(used) * u128::from(MILLION)).checked_div(u128::from(g.memory)).map_or(0, |share| u64::try_from(share).unwrap_or(u64::MAX));
    Some(memory.max(u64::from(compute) * 1000))
}

fn observed(g: &Gpu) -> Option<(u16, u64)> {
    Some((g.busy_per_mille.filter(|busy| *busy <= 1000)?, g.memory_used?))
}

fn read(facts: &RunnerFacts, g: &Gpu, name: &str) -> Fact {
    match name.to_ascii_lowercase().as_str() {
        "gpu.model" => Fact::List(vec![g.model.clone(), g.model.split_whitespace().collect::<Vec<_>>().join("-")]),
        "gpu.vendor" => Fact::List(vec![g.vendor.clone()]),
        "gpu.memory.total" => Fact::Num(g.memory as f64),
        "gpu.memory" | "gpu.memory.free" => g.memory_used.map_or(Fact::Missing, |used| Fact::Num(g.memory.saturating_sub(used) as f64)),
        "gpu.utilization" => g.busy_per_mille.filter(|busy| *busy <= 1000).map_or(Fact::Missing, |busy| Fact::Num(f64::from(busy))),
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
    // A declared count without inventory (older peers and imported traces) still reserves GPUs.
    // It cannot satisfy a predicate on a particular card.
    if facts.gpus.is_empty() {
        return !device_predicates(&cs) && cs.iter().all(|(_, c)| c.eval(facts, labels));
    }
    matching(&cs, facts, labels).count() >= usize::try_from(needs.request.gpu).unwrap_or(usize::MAX)
}

fn device_predicates(cs: &[(String, Constraint)]) -> bool {
    cs.iter().any(|(_, c)| c.facts().iter().any(|name| name.to_ascii_lowercase().starts_with("gpu.") && !name.eq_ignore_ascii_case("gpu.count")))
}

fn matching<'a>(cs: &'a [(String, Constraint)], facts: &'a RunnerFacts, labels: &'a [String]) -> impl Iterator<Item = &'a Gpu> {
    facts.gpus.iter().filter(move |g| observed(g).is_some() && cs.iter().all(|(_, c)| c.eval_with(&|name| read(facts, g, name), labels)))
}

/// Hex model::bind: compatible free cards, utilization then used-memory then inventory position.
/// Pressure ranking is deliberately a separate step; sorting by dominant pressure is different.
pub fn bind(needs: &Needs, facts: &RunnerFacts, labels: &[String], free: impl Fn(usize, &Gpu) -> bool) -> Option<Vec<usize>> {
    let n = usize::try_from(needs.request.gpu).ok()?;
    if n == 0 { return Some(Vec::new()); }
    let cs = needs.constraints().ok()?;
    if facts.gpus.is_empty() {
        return (!device_predicates(&cs) && cs.iter().all(|(_, c)| c.eval(facts, labels))).then(Vec::new);
    }
    let mut fit: Vec<(u16, u64, usize)> = facts.gpus.iter().enumerate()
        .filter(|(i, g)| free(*i, g) && cs.iter().all(|(_, c)| c.eval_with(&|name| read(facts, g, name), labels)))
        .filter_map(|(i, g)| observed(g).map(|(compute, used)| (compute, used, i))).collect();
    fit.sort_unstable();
    (fit.len() >= n).then(|| fit.into_iter().take(n).map(|(_, _, i)| i).collect())
}

/// Hex index::gpu_idle: sum the selected cards' idle dominant shares. Both independent counters
/// must have been observed for fresh selection; unread cards earn no idle credit.
pub fn rank(needs: &Needs, facts: &RunnerFacts, labels: &[String], free: u64) -> Option<u64> {
    if needs.request.gpu == 0 { return Some(0); }
    if free < u64::from(needs.request.gpu) { return None; }
    if facts.gpus.is_empty() {
        bind(needs, facts, labels, |_, _| true)?;
        return Some(u64::from(needs.request.gpu).saturating_mul(MILLION));
    }
    // The existing wire carries reserved counts, not selected identities. Until Hex's durable
    // binding/attachment path is finished and ported, conservatively account for every held card.
    let held = facts.gpus.len().saturating_sub(usize::try_from(free).unwrap_or(usize::MAX));
    let mut capacity = needs.clone();
    capacity.request.gpu = capacity.request.gpu.checked_add(u32::try_from(held).ok()?)?;
    let selected = bind(&capacity, facts, labels, |_, _| true)?;
    Some(selected.into_iter().skip(held).filter_map(|i| facts.gpus.get(i)).filter_map(busy).map(|busy| MILLION.saturating_sub(busy)).fold(0, u64::saturating_add))
}

/// A pressure predicate can become true after other work ends; it must queue rather than be Never.
pub fn could_match(needs: &Needs, facts: &RunnerFacts, labels: &[String]) -> bool {
    if needs.request.gpu == 0 { return matches(needs, facts, labels); }
    fn possible(c: &Constraint, facts: &RunnerFacts, g: &Gpu, labels: &[String]) -> Option<bool> {
        match c {
            Constraint::And(cs) => { let values: Vec<_> = cs.iter().map(|c| possible(c, facts, g, labels)).collect(); if values.contains(&Some(false)) { Some(false) } else if values.contains(&None) { None } else { Some(true) } }
            Constraint::Or(cs) => { let values: Vec<_> = cs.iter().map(|c| possible(c, facts, g, labels)).collect(); if values.contains(&Some(true)) { Some(true) } else if values.contains(&None) { None } else { Some(false) } }
            Constraint::Not(c) => possible(c, facts, g, labels).map(|v| !v),
            _ if c.facts().iter().any(|n| matches!(n.as_str(), "gpu.memory" | "gpu.memory.free" | "gpu.utilization")) => None,
            _ => Some(c.eval_with(&|name| read(facts, g, name), labels)),
        }
    }
    let Ok(cs) = needs.constraints() else { return false };
    if facts.gpus.is_empty() { return matches(needs, facts, labels); }
    facts.gpus.iter().filter(|g| cs.iter().all(|(_, c)| possible(c, facts, g, labels) != Some(false))).count() >= usize::try_from(needs.request.gpu).unwrap_or(usize::MAX)
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
    fn w05_r22_gpu_predicates_must_hold_on_each_requested_card() -> Result<(), String> {
        let mut f = RunnerFacts { gpus: vec![card("A100", 8, Some(0), Some(0)), card("H100", 80, Some(0), Some(0))], ..RunnerFacts::default() };
        let n = needs::parse("gpus=1,gpu.model==A100,gpu.memory.total>=16Gi")?;
        assert!(!matches(&n, &f, &[])); // Aggregate model + maximum memory was a false fit.
        f.gpus.push(card("A100", 80, Some(64), Some(100)));
        assert!(matches(&n, &f, &[]));
        let two = needs::parse("gpus=2,gpu.model==A100,gpu.memory.free>=16Gi")?;
        assert!(!matches(&two, &f, &[])); // One qualifying card cannot satisfy two GPUs.
        let mut one = two.clone(); one.request.gpu = 1;
        assert!(matches(&one, &f, &[]));
        Ok(())
    }
    #[test]
    fn w05_r23_gpu_rank_uses_dominant_pressure_and_matching_cards_only() -> Result<(), String> {
        let n = needs::parse("gpus=1,gpu.model==A100")?;
        let f = RunnerFacts { gpus: vec![card("A100", 80, Some(72), Some(10)), card("H100", 80, Some(0), Some(0))], ..RunnerFacts::default() };
        assert_eq!(rank(&n, &f, &[], 2), Some(100_000));
        assert_eq!(rank(&n, &f, &[], 1), None); // Conservative until identities are on the live path.
        assert_eq!(busy(&card("A100", 80, Some(8), Some(900))), Some(900_000));
        assert_eq!(busy(&card("A100", 80, Some(100), Some(200))), Some(1_250_000));
        assert_eq!(busy(&card("A100", 80, Some(100), Some(2000))), None);
        Ok(())
    }
    #[test]
    fn w13_r01_missing_gpu_measurements_are_not_fabricated_idle() -> Result<(), String> {
        let unknown = card("A100", 80, None, None);
        assert_eq!(busy(&unknown), None);
        let f = RunnerFacts { gpus: vec![unknown], ..RunnerFacts::default() };
        let n = needs::parse("gpus=1")?;
        assert_eq!(rank(&n, &f, &[], 1), None);
        assert!(!matches(&needs::parse("gpus=1,gpu.memory.free>=1Gi")?, &f, &[]));
        assert!(could_match(&n, &f, &[]));
        Ok(())
    }
    #[test]
    fn w05_r22_gpu_boolean_constraints_keep_host_facts_and_count() -> Result<(), String> {
        let f = RunnerFacts { os: "linux".into(), gpus: vec![card("NVIDIA A100", 80, Some(0), Some(0)), card("H100", 80, Some(0), Some(0))], ..RunnerFacts::default() };
        let n = Needs { request: super::super::state::Resources { gpu: 1, ..Default::default() }, requires: vec!["os == linux && gpu.count == 2 && (gpu.model == nvidia-a100 || gpu.model == H100)".into()], ..Needs::default() };
        assert!(matches(&n, &f, &[]));
        let mut bad = n; bad.requires.push("gpu.vendor == amd".into());
        assert!(!matches(&bad, &f, &[]));
        Ok(())
    }
    #[test]
    fn w05_r03_count_only_inventory_keeps_legacy_capacity_without_inventing_devices() -> Result<(), String> {
        let f = RunnerFacts::default();
        let mut n = needs::parse("gpus=2")?;
        assert!(matches(&n, &f, &[]));
        assert_eq!(rank(&n, &f, &[], 2), Some(2_000_000));
        assert_eq!(rank(&n, &f, &[], 1), None);
        n.requires.push("gpu.model == A100".into());
        assert!(!matches(&n, &f, &[]));
        assert_eq!(rank(&n, &f, &[], 4), None);
        Ok(())
    }
    #[test]
    fn w05_r23_device_selection_and_node_pressure_match_hex() -> Result<(), String> {
        let n = needs::parse("gpus=1,gpu.model==A100")?;
        let f = RunnerFacts { gpus: vec![card("A100", 80, Some(72), Some(100)), card("A100", 80, Some(0), Some(200))], ..RunnerFacts::default() };
        // Hex binds by utilization first, then scores the chosen card's dominant pressure.
        // Sorting cards directly by dominant pressure would pick card 1 and give 800,000.
        assert_eq!(bind(&n, &f, &[], |_, _| true), Some(vec![0]));
        assert_eq!(rank(&n, &f, &[], 2), Some(100_000));
        assert_eq!(bind(&n, &f, &[], |i, _| i != 0), Some(vec![1]));
        let n = needs::parse("gpus=1,gpu.memory>=16Gi")?;
        assert_eq!(bind(&n, &f, &[], |_, _| true), Some(vec![1]));
        assert!(could_match(&n, &f, &[]));
        Ok(())
    }
    #[test]
    fn w13_r01_gpu_facts_old_and_new_json_are_compatible() -> Result<(), String> {
        let old: Gpu = serde_json::from_str(r#"{"vendor":"nvidia","model":"A100","memory":80}"#).map_err(|e| e.to_string())?;
        assert_eq!(busy(&old), None);
        let json = serde_json::to_value(&old).map_err(|e| e.to_string())?;
        assert!(json.get("memory_used").is_none());
        assert!(json.get("busy_per_mille").is_none());
        assert_eq!(serde_json::from_value::<Gpu>(json).map_err(|e| e.to_string())?, old);
        Ok(())
    }

    /// W13-R01/W05-R03: unknown is not an idle sample, at the same fresh-fit seam as Hex b5b67b38.
    #[test]
    fn w13_r01_unread_gpu_load_never_becomes_fresh_idle_capacity() -> Result<(), String> {
        let n = needs::parse("gpus=1,gpu.model==A100,gpu.memory.free>=16Gi")?;
        for (used, compute) in [(None, None), (None, Some(0)), (Some(0), None), (Some(0), Some(1001))] {
            let f = RunnerFacts { gpus: vec![card("A100", 80, used, compute)], ..RunnerFacts::default() };
            assert_eq!(busy(f.gpus.first().ok_or("no card")?), None, "used={used:?}, compute={compute:?}");
            assert!(!matches(&n, &f, &[]), "unread card matched: used={used:?}, compute={compute:?}");
            assert_eq!(bind(&n, &f, &[], |_, _| true), None);
            assert_eq!(rank(&n, &f, &[], 1), None);
            assert!(could_match(&n, &f, &[]), "a fresh reading can make the card usable");
            assert!(matches(&Needs::default(), &f, &[]), "unread GPU counters must not block CPU work");
        }
        let known = RunnerFacts { gpus: vec![card("A100", 80, Some(0), Some(0))], ..RunnerFacts::default() };
        assert_eq!(rank(&n, &known, &[], 1), Some(1_000_000));
        Ok(())
    }

    /// W05-R03/W05-R23: legacy counts preserve admission, but are not measured idle capacity.
    #[test]
    fn w05_r23_legacy_gpu_counts_do_not_earn_fabricated_idle_credit() -> Result<(), String> {
        let n = needs::parse("gpus=1")?;
        let legacy = RunnerFacts::default();
        assert!(matches(&n, &legacy, &[]));
        assert_eq!(bind(&n, &legacy, &[], |_, _| true), Some(Vec::new()), "no physical identity is invented");
        assert_eq!(rank(&n, &legacy, &[], 1), Some(0), "a declared count is not an observed idle GPU");
        let measured = RunnerFacts { gpus: vec![card("A100", 80, Some(0), Some(500))], ..RunnerFacts::default() };
        assert_eq!(rank(&n, &measured, &[], 1), Some(500_000));
        Ok(())
    }
}
