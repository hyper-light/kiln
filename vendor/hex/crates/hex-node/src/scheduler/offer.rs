//! AC-T7.4.1/LB03: live peer facts carry model names, never another process's model IDs.
//! Every encoded offer and the shared model table remain bounded by the common wire frame.
//! Runtime reservation/capacity changes are checked by the existing driver's refresh operation.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use hex_api::Refusal;
use hex_core::{schedule::Offer, wire::MAX_MESSAGE};
use hex_sim::model::{Class, Classes, Cluster, ModelId, NodeId, NodeSpec, model_name};
use serde::Serialize;

fn invalid(why: impl Into<String>) -> Refusal {
    Refusal::Invalid {
        what: "peer offer",
        why: why.into(),
    }
}

struct Bounded(Vec<u8>);
impl Write for Bounded {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self
            .0
            .len()
            .checked_add(bytes.len())
            .is_none_or(|n| n > MAX_MESSAGE)
        {
            return Err(std::io::Error::other(
                "peer offer exceeds the wire byte bound",
            ));
        }
        self.0.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encoded(value: &impl Serialize) -> Result<Vec<u8>, Refusal> {
    let mut out = Bounded(Vec::new());
    serde_json::to_writer(&mut out, value).map_err(|e| {
        if e.is_io() {
            Refusal::Exhausted {
                what: "peer offer bytes",
                bound: MAX_MESSAGE,
            }
        } else {
            invalid(e.to_string())
        }
    })?;
    Ok(out.0)
}

fn checked_name(name: &str) -> Result<(), Refusal> {
    if name.is_empty() || name == "-" || name.len() > MAX_MESSAGE || model_name(name) != name {
        return Err(invalid("model names must be nonempty canonical names"));
    }
    Ok(())
}

fn checked_shape(spec: &NodeSpec) -> Result<(), Refusal> {
    spec.check_runtime_pools()
        .map_err(|e| invalid(e.to_string()))?;
    for class in [Class::Firecracker, Class::Shards] {
        if spec.routes.is_none()
            && spec.classes.has(class)
            && spec.costs.get(class.index()).is_none_or(Option::is_none)
        {
            return Err(invalid(
                "initialized VM peer offer has no immutable runtime cost",
            ));
        }
    }
    if !spec.devices_agree() {
        return Err(invalid("device list and GPU capacity disagree"));
    }
    let mut classes = Classes::NONE;
    for class in [Class::Oci, Class::Firecracker, Class::Shards] {
        if spec.classes.has(class) {
            classes = classes.with(class);
        }
    }
    if classes != spec.classes {
        return Err(invalid("unknown runtime class bits"));
    }
    if spec
        .devices
        .iter()
        .any(|d| d.load.busy_per_mille.is_some_and(|busy| busy > 1000))
    {
        // The field is a fraction in thousandths, not an OS load-average queue length.
        return Err(invalid(
            "device busy fraction exceeds one thousand per mille",
        ));
    }
    Ok(())
}

/// Packs only names this node actually references, remapping its device IDs into that offer's
/// compact table. UNNAMED remains unnamed; no missing hardware fact is invented.
pub fn from_local(spec: &NodeSpec, models: &[String]) -> Result<Offer, Refusal> {
    encoded(spec)?;
    checked_shape(spec)?;
    let mut spec = spec.clone();
    let mut names = BTreeMap::<String, ModelId>::new();
    let mut models_out = Vec::new();
    for device in &mut spec.devices {
        if device.model == ModelId::UNNAMED {
            continue;
        }
        let source = usize::try_from(device.model.0)
            .ok()
            .and_then(|at| models.get(at))
            .ok_or_else(|| invalid("device references an unknown local model"))?;
        checked_name(source)?;
        let model = if let Some(model) = names.get(source) {
            *model
        } else {
            let model = u32::try_from(models_out.len())
                .ok()
                .filter(|n| *n != ModelId::UNNAMED.0)
                .map(ModelId)
                .ok_or_else(|| invalid("offer model ID is unrepresentable"))?;
            names.insert(source.clone(), model);
            models_out.push(source.clone());
            // Check during construction too: many individually valid long model names must not
            // accumulate a larger temporary table than the frame can carry.
            encoded(&models_out)?;
            model
        };
        device.model = model;
    }
    let spec = String::from_utf8(encoded(&spec)?).map_err(|e| invalid(e.to_string()))?;
    let offer = Offer {
        spec,
        models: models_out,
    };
    encoded(&offer)?;
    Ok(offer)
}

/// Authenticated `expected` binds the advertised node. No cluster mutation occurs until the full
/// offer, names, references and shared-table byte budget have passed validation. Existing held
/// device identity and capacity are subsequently checked by Driver::refresh_node.
pub fn decode(offer: &Offer, expected: NodeId, cluster: &mut Cluster) -> Result<NodeSpec, Refusal> {
    encoded(offer)?;
    let mut spec: NodeSpec =
        serde_json::from_str(&offer.spec).map_err(|e| invalid(e.to_string()))?;
    if spec.id != expected {
        return Err(invalid("advertised node is not the authenticated sender"));
    }
    if !cluster.nodes.iter().any(|node| node.id == expected) {
        return Err(invalid("advertised node is not a configured peer"));
    }
    checked_shape(&spec)?;
    let mut unique = BTreeSet::new();
    for name in &offer.models {
        checked_name(name)?;
        if !unique.insert(name) {
            return Err(invalid("duplicate model names"));
        }
    }
    // The shared name table is a wire-sized restart/exchange input too. Check it before cloning;
    // remote hotplug cannot grow a process-lifetime intern table without a bound.
    encoded(&cluster.models)?;
    let mut staged = Cluster::single_owner(Vec::new(), Vec::new());
    staged.models = cluster.models.clone();
    for device in &mut spec.devices {
        if device.model == ModelId::UNNAMED {
            continue;
        }
        let name = usize::try_from(device.model.0)
            .ok()
            .and_then(|at| offer.models.get(at))
            .ok_or_else(|| invalid("device references an unknown offered model"))?;
        device.model = staged.model(name).map_err(|e| invalid(e.to_string()))?;
    }
    encoded(&staged.models)?;
    cluster.models = staged.models;
    Ok(spec)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_sim::model::{Device, Resources};

    fn node() -> NodeSpec {
        let mut node = NodeSpec::new(
            NodeId(1),
            Resources {
                cpu_milli: 4_000,
                mem_bytes: 8 << 30,
                gpus: 2,
                ..Resources::default()
            },
        );
        node.classes = Classes::NONE.with(Class::Oci);
        node.devices = vec![
            Device::gpu(ModelId(0), 8 << 30),
            Device::gpu(ModelId(1), 16 << 30),
        ];
        node
    }

    /// AC-T7.4.1: existing host-backed records with no runtime-pool field retain their meaning.
    #[test]
    fn a_record_without_runtime_pools_remains_host_backed() {
        let original = node();
        let encoded = serde_json::to_value(&original).unwrap();
        assert!(encoded.get("runtime_pools").is_none());
        let restored: NodeSpec = serde_json::from_value(encoded).unwrap();
        assert!(restored.runtime_pools.is_empty());
        assert_eq!(restored, original);
    }

    /// AC-T7.4.1: malformed runtime pools are refused before a peer can change shared models.
    #[test]
    fn invalid_runtime_pool_does_not_mutate_shared_models() {
        let mut source = node();
        source.runtime_pools.push(hex_sim::model::RuntimePool {
            classes: Classes::NONE,
            capacity: source.capacity,
        });
        let incoming = Offer {
            spec: serde_json::to_string(&source).unwrap(),
            models: vec!["new-model".into()],
        };
        let mut cluster = Cluster::single_owner(vec![source], Vec::new());
        cluster.models.push("kept-model".into());
        assert!(decode(&incoming, NodeId(1), &mut cluster).is_err());
        assert_eq!(cluster.models, ["kept-model"]);
    }

    /// AC-T7.4.1: reordered intern tables preserve the exact physical model and device order.
    #[test]
    fn peer_models_are_remapped_by_name_not_sender_number() {
        let source = node();
        let names = vec!["nvidia-rtx".into(), "amd-radeon".into()];
        let offer = from_local(&source, &names).unwrap();
        let mut cluster = Cluster::single_owner(
            vec![
                NodeSpec::new(NodeId(0), Resources::default()),
                source.clone(),
            ],
            Vec::new(),
        );
        cluster.models = vec!["amd-radeon".into(), "nvidia-rtx".into()];
        let actual = decode(&offer, NodeId(1), &mut cluster).unwrap();
        assert_eq!(actual.devices[0].model, ModelId(1));
        assert_eq!(actual.devices[1].model, ModelId(0));
        assert_eq!(actual.devices[0].size, source.devices[0].size);
        assert_eq!(actual.capacity, source.capacity);
        assert_eq!(cluster.models.len(), 2);
    }

    /// AC-T7.4.1: an advertised identity cannot overwrite another configured node.
    #[test]
    fn advertised_identity_is_checked_before_interning() {
        let source = node();
        let offer = from_local(&source, &["nvidia-rtx".into(), "amd-radeon".into()]).unwrap();
        let mut cluster = Cluster::single_owner(
            vec![NodeSpec::new(NodeId(0), Resources::default()), source],
            Vec::new(),
        );
        assert!(decode(&offer, NodeId(0), &mut cluster).is_err());
        assert!(cluster.models.is_empty());
    }

    /// AC-T7.4.1: mismatched capacity, unknown references and class bits are typed input failures.
    #[test]
    fn invalid_device_capacity_and_references_do_not_mutate_model_table() {
        let source = node();
        let mut cluster = Cluster::single_owner(
            vec![
                NodeSpec::new(NodeId(0), Resources::default()),
                source.clone(),
            ],
            Vec::new(),
        );
        let mut bad = source.clone();
        bad.capacity.gpus = 1;
        assert!(from_local(&bad, &["nvidia-rtx".into(), "amd-radeon".into()]).is_err());
        let mut offer = from_local(&source, &["nvidia-rtx".into(), "amd-radeon".into()]).unwrap();
        offer.models.pop();
        assert!(decode(&offer, NodeId(1), &mut cluster).is_err());
        assert!(cluster.models.is_empty());
        let mut value = serde_json::to_value(&source).unwrap();
        value["classes"] = serde_json::json!(8);
        let offer = Offer {
            spec: serde_json::to_string(&value).unwrap(),
            models: vec!["nvidia-rtx".into(), "amd-radeon".into()],
        };
        assert!(decode(&offer, NodeId(1), &mut cluster).is_err());
        assert!(cluster.models.is_empty());
    }

    /// AC-T7.4.1: a frame-sized budget bounds both offered text and the interned shared names.
    #[test]
    fn oversized_offer_is_refused_before_parsing_and_unused_names_are_not_interned() {
        let source = node();
        let mut cluster = Cluster::single_owner(
            vec![
                NodeSpec::new(NodeId(0), Resources::default()),
                source.clone(),
            ],
            Vec::new(),
        );
        let oversized = Offer {
            spec: "x".repeat(MAX_MESSAGE + 1),
            models: Vec::new(),
        };
        assert!(matches!(
            decode(&oversized, NodeId(1), &mut cluster),
            Err(Refusal::Exhausted { .. })
        ));
        let mut offer = from_local(&source, &["nvidia-rtx".into(), "amd-radeon".into()]).unwrap();
        offer.models.push("unused".into());
        decode(&offer, NodeId(1), &mut cluster).unwrap();
        assert!(!cluster.models.iter().any(|n| n == "unused"));
    }
}
