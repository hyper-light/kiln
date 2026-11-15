//! Joint plan step 14 (AC-T7.4.1; `docs/design/learned-charge.md`): what the scheduler takes of its node's charge
//! learner. The learner lives in the node agent (`crate::node::charges`), the one authority, as the owner of its
//! microVMs' cgroups, and its capacity answers carry what it learned. Each refresh the scheduler gives its node's
//! facts the priors those answers carry (C2), which the placement index books a new VM's receipt at, and books each
//! VM it runs at the holding the answers carry (goal 13's `Driver::rebook`). The node's capacity stays the
//! machine's (C8).

use std::collections::BTreeMap;

use hex_api::{BoxId, CapacityResources, Learned, Refusal};
use hex_sim::model::{Class, NodeSpec, Prior};
use hex_sim::peer::Key;

/// What each VM class's capacity answer learned, taken before the answers are applied.
pub(super) fn learned(
    reports: &[(Class, Option<hex_api::Capacity>)],
) -> Result<Vec<(Class, Learned)>, Refusal> {
    let mut out = Vec::new();
    out.try_reserve_exact(reports.len())
        .map_err(|_| Refusal::Exhausted {
            what: "learned capacity answers",
            bound: reports.len(),
        })?;
    for (class, capacity) in reports {
        if let Some(learned) = capacity.as_ref().and_then(|c| c.learned.as_ref()) {
            out.push((*class, learned.try_clone()?));
        }
    }
    Ok(out)
}

/// Gives `local` the priors `learned` carries (C2): each VM class's prior for the first of its cost profiles (its own,
/// then its routes') whose VMs have warmed, as the answer for that profile and its ceiling says it.
pub(super) fn apply(local: &mut NodeSpec, learned: &[(Class, Learned)]) -> Result<(), Refusal> {
    let mut profiles: Vec<(Class, hex_api::RuntimeCost)> = Vec::new();
    for class in [Class::Shards, Class::Firecracker] {
        if let Some(cost) = local.costs.get(class.index()).copied().flatten() {
            profiles.push((class, cost));
        }
    }
    if let Some(routes) = &local.routes {
        for offer in &routes.offers {
            let class = match offer.key.class {
                hex_api::Class::Shards => Class::Shards,
                hex_api::Class::Firecracker => Class::Firecracker,
                hex_api::Class::Process | hex_api::Class::Oci => continue,
            };
            if let Some(cost) = offer.cost {
                profiles.push((class, cost));
            }
        }
    }
    local.priors = Default::default();
    for (class, cost) in profiles {
        let ceiling = cost.allocate(CapacityResources::default())?.resources;
        let prior = learned
            .iter()
            .filter(|(of, _)| *of == class)
            .flat_map(|(_, learned)| &learned.priors)
            .find(|prior| prior.profile == cost.id && prior.ceiling == ceiling);
        let slot = local
            .priors
            .get_mut(class.index())
            .ok_or_else(|| Refusal::Broken("a runtime class past the classes".into()))?;
        if let (Some(prior), None) = (prior, slot.as_ref()) {
            *slot = Some(Prior {
                profile: cost.id,
                booking: prior.booking,
                ceiling,
            });
        }
    }
    Ok(())
}

/// The holdings `learned` carries, each as the holder of the node's ledger it names (C8; joint plan step 39): a box
/// against its key among `ids`, a node VM by its run. A holding of a box this node doesn't run under its scheduler is no
/// ledger's, and is passed over.
pub(super) fn holds(
    learned: &[(Class, Learned)],
    ids: &BTreeMap<BoxId, Key>,
) -> Result<Vec<(hex_sim::peer::Holder, CapacityResources)>, Refusal> {
    let count = learned
        .iter()
        .map(|(_, learned)| learned.holdings.len())
        .fold(0usize, usize::saturating_add);
    let mut out = Vec::new();
    out.try_reserve_exact(count)
        .map_err(|_| Refusal::Exhausted {
            what: "learned holdings",
            bound: count,
        })?;
    for holding in learned.iter().flat_map(|(_, learned)| &learned.holdings) {
        let holder = match &holding.id {
            hex_api::LearnedHolder::Box(id) => match ids.get(id) {
                Some(key) => hex_sim::peer::Holder::Box(*key),
                // A box the node's scheduler doesn't run is no holder of its ledger's.
                None => continue,
            },
            hex_api::LearnedHolder::Node(run) => hex_sim::peer::Holder::Node(*run),
        };
        out.push((holder, holding.holding));
    }
    Ok(out)
}

/// The node VMs `reports` name that hold room (joint plan step 39; ROOT, 2026-11-13: a node VM's receipt reaches the
/// scheduler on its node's capacity answer): each Booting or Ready one by its run, at what its receipt books. An Ended
/// one holds none. One node's records name each of its node VMs once, so answers naming a run twice are refused.
pub(super) fn node_vms(
    reports: &[(Class, Option<hex_api::Capacity>)],
) -> Result<BTreeMap<hex_api::NodeRun, hex_sim::model::Resources>, Refusal> {
    let mut named = std::collections::BTreeSet::new();
    let mut booked = BTreeMap::new();
    for receipt in reports
        .iter()
        .filter_map(|(_, capacity)| capacity.as_ref())
        .flat_map(|capacity| &capacity.node_vms)
    {
        if !named.insert(receipt.run) {
            return Err(Refusal::Invalid {
                what: "node VM receipts",
                why: "two capacity answers name one node VM".to_owned(),
            });
        }
        if matches!(
            receipt.phase,
            hex_api::NodeVmPhase::Booting | hex_api::NodeVmPhase::Ready
        ) {
            booked.insert(
                receipt.run,
                hex_sim::model::Resources {
                    cpu_milli: receipt.booking.cpu_milli,
                    mem_bytes: receipt.booking.mem_bytes,
                    disk_bytes: receipt.booking.disk_bytes,
                    gpus: 0,
                },
            );
        }
    }
    Ok(booked)
}

#[cfg(test)]
mod tests;
