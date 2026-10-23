//! `azure-vmss` (docs/cd-targets.md §3): a virtual machine scale set, its model moved to each
//! release's image and rolled out by its rolling upgrade policy.
//!
//! ```yaml
//! web:
//!   azure-vmss:
//!     scale-set: web
//!     image: "/subscriptions/…/galleries/acme/images/web/versions/${{ release.version }}"   # or publisher:offer:sku:version
//!     spec: deploy/scaleset.json            # the scale set as Resource Manager spells it, for making it
//!     upgrade: { maxBatchInstancePercent: 20, pauseTimeBetweenBatches: PT30S }
//! ```
//!
//! - **ScaleSet** (`web`): `spec` is the scale set as Resource Manager spells it, its image at
//!   `properties.virtualMachineProfile.storageProfile.imageReference` and its upgrade policy
//!   `Rolling`, with `upgrade:`'s settings: Azure moves the instances onto the new model in
//!   batches, and a batch whose instances fail stops it (`Faulted`). A rollback moves the model
//!   back to the earlier image and rolls that out the same way.
//! - **Health** is the instances': all on the model, or the rolling upgrade's progress and why it
//!   stopped.

use serde_json::{Map, Value, json};

use super::{RenderInput, apply_sets, file_or_object, file_settings, object, status, text};
use crate::health::Status;
use crate::model::Health;

pub const GROUP: &str = "compute.azure.kiln.dev";

/// Resource Manager's API version kiln speaks for scale sets.
pub const API: &str = "2024-07-01";

const OUTPUT: &[&str] = &["id", "name", "type", "etag", "systemData"];
const OUTPUT_PROPS: &[&str] = &["provisioningState", "uniqueId", "timeCreated"];

/// The files a render reads.
pub fn files(spec: &Value) -> Vec<String> {
    file_settings(spec, &["spec"])
}

/// An image as `image:` names it: an image's or a gallery image version's id, or a marketplace
/// image (`publisher:offer:sku:version`).
pub fn image_reference(image: &str) -> Result<Value, String> {
    if image.starts_with("/subscriptions/") {
        return Ok(json!({ "id": image }));
    }
    match image.split(':').collect::<Vec<_>>().as_slice() {
        [publisher, offer, sku, version] if [publisher, offer, sku, version].iter().all(|p| !p.is_empty()) => Ok(json!({ "publisher": publisher, "offer": offer, "sku": sku, "version": version })),
        _ => Err(format!("image `{image}`: an image's id (/subscriptions/…), or publisher:offer:sku:version")),
    }
}

/// An app's objects.
pub fn render(input: &RenderInput<'_>) -> Result<Vec<Value>, String> {
    let name = text(input.spec, "scale-set").ok_or("azure-vmss needs scale-set: the scale set's name")?;
    if name.is_empty() || name.len() > 64 || !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) {
        return Err(format!("scale-set `{name}`: up to 64 letters, digits, -, _ and ."));
    }
    let image = image_reference(text(input.spec, "image").ok_or("azure-vmss needs image: the image each release runs")?)?;
    let mut res = match file_or_object(input, "spec")? {
        Some(v @ Value::Object(_)) => v,
        Some(_) => return Err("spec: the scale set, as a file or an object".into()),
        None => json!({}),
    };
    if let Some(m) = res.as_object_mut() {
        for k in OUTPUT {
            m.remove(*k);
        }
    }
    let props = crate::obj::ensure_map(&mut res, &["properties"]).ok_or("spec.properties: an object")?;
    for k in OUTPUT_PROPS {
        props.remove(*k);
    }
    let storage = crate::obj::ensure_map(&mut res, &["properties", "virtualMachineProfile", "storageProfile"]).ok_or("the scale set's storageProfile: an object")?;
    storage.insert("imageReference".into(), image);
    let policy = crate::obj::ensure_map(&mut res, &["properties", "upgradePolicy"]).ok_or("properties.upgradePolicy: an object")?;
    policy.insert("mode".into(), json!("Rolling"));
    if let Some(Value::Object(u)) = input.spec.get("upgrade") {
        let rolling = policy.entry("rollingUpgradePolicy").or_insert_with(|| json!({}));
        if let Value::Object(r) = rolling {
            r.extend(u.clone());
        }
    }
    apply_sets(&mut res, input.spec.get("set"))?;
    Ok(vec![object(GROUP, "ScaleSet", name, None, res)])
}

/// A ScaleSet object from Azure's answer, with how many instances run its model (`model`:
/// `[on it, of]`) and its latest rolling upgrade (`upgrade`), as the server reads them.
pub fn live_scale_set(answer: &Value, model: Option<(u64, u64)>, upgrade: Option<&Value>) -> Option<Value> {
    let name = answer.get("name")?.as_str()?;
    let mut spec = answer.as_object()?.clone();
    for k in OUTPUT {
        spec.remove(*k);
    }
    let state = spec.get_mut("properties").and_then(Value::as_object_mut).map(|p| {
        let s = p.get("provisioningState").cloned();
        for k in OUTPUT_PROPS {
            p.remove(*k);
        }
        s
    });
    let app = spec.get_mut("tags").and_then(Value::as_object_mut).and_then(|t| t.remove("kiln-app"));
    spec.get_mut("tags").and_then(Value::as_object_mut).map(|t| t.remove("kiln-tracking"));
    if spec.get("tags").and_then(Value::as_object).is_some_and(Map::is_empty) {
        spec.remove("tags");
    }
    let mut o = object(GROUP, "ScaleSet", name, None, Value::Object(spec));
    if let Some(Value::String(a)) = app
        && let Some(l) = crate::obj::ensure_map(&mut o, &["metadata", "labels"])
    {
        l.insert(crate::engine::APP_LABEL.into(), json!(a));
    }
    let up = upgrade.map(|u| json!({ "code": u.pointer("/properties/runningStatus/code"), "progress": u.pointer("/properties/progress"), "error": u.pointer("/properties/error/message") }));
    if let Value::Object(m) = &mut o {
        m.insert("status".into(), json!({ "provisioningState": state.flatten(), "onModel": model.map(|(n, _)| n), "instances": model.map(|(_, n)| n), "upgrade": up }));
    }
    Some(o)
}

pub fn health(o: &Value) -> Option<Status> {
    if crate::obj::kind(o) != "ScaleSet" {
        return None;
    }
    let st = o.get("status")?;
    match st.get("provisioningState").and_then(Value::as_str).unwrap_or("Succeeded") {
        "Failed" => return status(Health::Degraded, "Azure couldn't provision the scale set"),
        "Succeeded" => {}
        s => return status(Health::Progressing, format!("the scale set is {s}")),
    }
    let (on, of) = (st.get("onModel").and_then(Value::as_u64), st.get("instances").and_then(Value::as_u64));
    if on.is_some() && on == of {
        return status(Health::Healthy, "");
    }
    match st.pointer("/upgrade/code").and_then(Value::as_str) {
        Some(c @ ("Faulted" | "Cancelled")) => status(Health::Degraded, format!("its rolling upgrade {}: {}", c.to_lowercase(), st.pointer("/upgrade/error").and_then(Value::as_str).unwrap_or("instances failed"))),
        _ => status(Health::Progressing, format!("rolling out: {} of {} instances on the model", on.unwrap_or(0), of.unwrap_or(0))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn images_and_policies_render() -> Result<(), String> {
        static EMPTY: std::sync::LazyLock<Map<String, Value>> = std::sync::LazyLock::new(Map::new);
        let read = |_: &str| None;
        let spec = json!({ "scale-set": "web", "image": "Canonical:ubuntu-24_04-lts:server:latest", "upgrade": { "maxBatchInstancePercent": 50 }, "spec": { "sku": { "name": "Standard_B2s", "capacity": 2 } } });
        let objs = render(&RenderInput { spec: &spec, target: &EMPTY, app: "default/web-staging", namespace: "", revision: "c0ffee", read: &read })?;
        let o = objs.first().ok_or("no scale set")?;
        assert_eq!(o.pointer("/spec/properties/virtualMachineProfile/storageProfile/imageReference"), Some(&json!({ "publisher": "Canonical", "offer": "ubuntu-24_04-lts", "sku": "server", "version": "latest" })));
        assert_eq!(o.pointer("/spec/properties/upgradePolicy"), Some(&json!({ "mode": "Rolling", "rollingUpgradePolicy": { "maxBatchInstancePercent": 50 } })));
        assert!(image_reference("ubuntu").is_err());
        let live = |on: u64, code: &str| json!({ "kind": "ScaleSet", "status": { "provisioningState": "Succeeded", "onModel": on, "instances": 2, "upgrade": { "code": code, "error": "instance 1 failed" } } });
        assert_eq!(health(&live(2, "Completed")).map(|s| s.health), Some(Health::Healthy));
        assert_eq!(health(&live(1, "RollingForward")).map(|s| s.health), Some(Health::Progressing));
        assert_eq!(health(&live(0, "Faulted")).map(|s| s.health), Some(Health::Degraded));
        Ok(())
    }
}
