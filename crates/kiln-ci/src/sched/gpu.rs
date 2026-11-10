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
        "gpu.memory.total" => Fact::Num(g.memory as f64),
        "gpu.memory" | "gpu.memory.free" => Fact::Num(g.memory.saturating_sub(g.memory_used.unwrap_or(0)) as f64),
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
    facts.gpus.iter().filter(move |g| cs.iter().all(|(_, c)| c.eval_with(&|name| read(facts, g, name), labels)))
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
        .map(|(i, g)| (g.busy_per_mille.unwrap_or(0), g.memory_used.unwrap_or(0), i)).collect();
    fit.sort_unstable();
    (fit.len() >= n).then(|| fit.into_iter().take(n).map(|(_, _, i)| i).collect())
}

/// Hex index::gpu_idle: sum the selected cards' idle dominant shares. A failed counter contributes
/// no component, exactly as Hex's gpu_facts conversion; it remains optional in the reported facts.
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
    Some(selected.into_iter().skip(held).filter_map(|i| facts.gpus.get(i)).map(|g| MILLION.saturating_sub(busy(g).unwrap_or(0))).fold(0, u64::saturating_add))
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
    fn w05_r22_gpu_predicates_must_hold_on_each_requested_card() {
        let mut f = RunnerFacts { gpus: vec![card("A100", 8, Some(0), Some(0)), card("H100", 80, Some(0), Some(0))], ..RunnerFacts::default() };
        let n = needs::parse("gpus=1,gpu.model==A100,gpu.memory.total>=16Gi").unwrap();
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
        assert_eq!(rank(&n, &f, &[], 2), Some(100_000));
        assert_eq!(rank(&n, &f, &[], 1), None); // Conservative until identities are on the live path.
        assert_eq!(busy(&card("A100", 80, Some(8), Some(900))), Some(900_000));
        assert_eq!(busy(&card("A100", 80, Some(100), Some(2000))), Some(1_000_000));
    }
    #[test]
    fn w13_r01_missing_gpu_measurements_are_not_fabricated_idle() {
        let unknown = card("A100", 80, None, None);
        assert_eq!(busy(&unknown), None);
        let f = RunnerFacts { gpus: vec![unknown], ..RunnerFacts::default() };
        let n = needs::parse("gpus=1").unwrap();
        assert_eq!(rank(&n, &f, &[], 1), Some(1_000_000));
        assert!(matches(&needs::parse("gpus=1,gpu.memory.free>=1Gi").unwrap(), &f, &[]));
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
    fn w05_r03_count_only_inventory_keeps_legacy_capacity_without_inventing_devices() {
        let f = RunnerFacts::default();
        let mut n = needs::parse("gpus=2").unwrap();
        assert!(matches(&n, &f, &[]));
        assert_eq!(rank(&n, &f, &[], 2), Some(2_000_000));
        assert_eq!(rank(&n, &f, &[], 1), None);
        n.requires.push("gpu.model == A100".into());
        assert!(!matches(&n, &f, &[]));
        assert_eq!(rank(&n, &f, &[], 4), None);
    }
    #[test]
    fn w05_r23_device_selection_and_node_pressure_match_hex() {
        let n = needs::parse("gpus=1,gpu.model==A100").unwrap();
        let f = RunnerFacts { gpus: vec![card("A100", 80, Some(72), Some(100)), card("A100", 80, Some(0), Some(200))], ..RunnerFacts::default() };
        // Hex binds by utilization first, then scores the chosen card's dominant pressure.
        // Sorting cards directly by dominant pressure would pick card 1 and give 800,000.
        assert_eq!(bind(&n, &f, &[], |_, _| true), Some(vec![0]));
        assert_eq!(rank(&n, &f, &[], 2), Some(100_000));
        assert_eq!(bind(&n, &f, &[], |i, _| i != 0), Some(vec![1]));
        let n = needs::parse("gpus=1,gpu.memory>=16Gi").unwrap();
        assert_eq!(bind(&n, &f, &[], |_, _| true), Some(vec![1]));
        assert!(could_match(&n, &f, &[]));
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
