//! AC-T7.4.1: bind the existing receiver decision to the native Runtime, never choose twice.

use super::*;

pub(super) fn physical(
    selected: Vec<hex_sim::model::Device>,
) -> Result<Vec<hex_api::DeviceId>, Error> {
    let mut ids = Vec::new();
    ids.try_reserve_exact(selected.len())
        .map_err(|_| Refusal::Exhausted {
            what: "physical device assignments",
            bound: selected.len(),
        })?;
    for device in selected {
        let kind = match device.kind {
            hex_sim::model::DeviceKind::Gpu => hex_api::DeviceKind::Gpu,
            hex_sim::model::DeviceKind::Vfio => hex_api::DeviceKind::Vfio,
            hex_sim::model::DeviceKind::Block => hex_api::DeviceKind::Block,
            hex_sim::model::DeviceKind::Net => hex_api::DeviceKind::Net,
        };
        let key = device
            .key
            .ok_or_else(|| invalid("a synthetic device cannot become a native assignment"))?;
        let id = hex_api::DeviceId { kind, key };
        id.check()?;
        ids.push(id);
    }
    Ok(ids)
}

/// The native record must describe exactly this reservation before any init starts or is resumed.
/// Record inspection is necessary recovery evidence; it does not prove hardware isolation by itself.
pub(super) fn verify(
    runtime: &Arc<dyn Runtime>,
    id: &BoxId,
    class: hex_api::Class,
    expected: &[hex_api::DeviceId],
) -> Result<(), Error> {
    if expected.is_empty() {
        return Ok(());
    }
    let report = runtime
        .capacity(class)?
        .ok_or_else(|| invalid("device-bearing runtime reports no binding metadata"))?;
    report.check()?;
    if expected.iter().any(|claim| {
        !report.devices.iter().any(|device| {
            device.id == *claim
                && device
                    .holder
                    .as_ref()
                    .and_then(hex_api::DeviceHolder::box_id)
                    == Some(id)
        })
    }) || report.devices.iter().any(|device| {
        device
            .holder
            .as_ref()
            .and_then(hex_api::DeviceHolder::box_id)
            == Some(id)
            && !expected.contains(&device.id)
    }) {
        return Err(
            bad("native device bindings disagree with the durable receiver assignment").into(),
        );
    }
    Ok(())
}
