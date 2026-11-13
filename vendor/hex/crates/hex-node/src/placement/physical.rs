//! One factual native owner across overlapping runtime routes; never a holder or inventory cache.

use hex_api::{BackendUnavailable, PhysicalDevice, PhysicalReport, Refusal};
use hex_sim::model::Class;

fn invalid(why: &'static str) -> Refusal {
    let mut text = String::new();
    if text.try_reserve_exact(why.len()).is_err() {
        return full(why.len());
    }
    text.push_str(why);
    Refusal::Invalid {
        what: "native physical reports",
        why: text,
    }
}

pub(super) fn add_unavailable(
    values: &mut Vec<BackendUnavailable>,
    observation: BackendUnavailable,
) -> Result<(), Refusal> {
    observation.check()?;
    if let Some(previous) = values
        .iter()
        .find(|value| value.backend == observation.backend)
    {
        return if previous == &observation {
            Ok(())
        } else {
            Err(invalid(
                "one observer backend has contradictory unavailable receipts",
            ))
        };
    }
    if values.len() >= BackendUnavailable::MAX_COUNT {
        return Err(full(BackendUnavailable::MAX_COUNT));
    }
    values
        .try_reserve_exact(1)
        .map_err(|_| full(BackendUnavailable::MAX_COUNT))?;
    values.push(observation);
    Ok(())
}

fn full(bound: usize) -> Refusal {
    Refusal::Exhausted {
        what: "native physical report entries",
        bound,
    }
}

pub(super) fn coalesce(reports: Vec<PhysicalReport>) -> Result<Option<PhysicalReport>, Refusal> {
    if reports.len() > Class::COUNT {
        return Err(invalid("more reports than configured backend classes"));
    }
    let mut authority = None;
    let mut devices: Vec<PhysicalDevice> = Vec::new();
    let mut unavailable = Vec::new();
    for report in reports {
        report.check()?;
        if report.authority.len() > hex_core::wire::MAX_MESSAGE {
            return Err(invalid("native authority exceeds the wire byte bound"));
        }
        if authority
            .as_ref()
            .is_some_and(|expected| expected != &report.authority)
        {
            return Err(invalid(
                "backend routes have different native record authorities",
            ));
        }
        if authority.is_none() {
            authority = Some(report.authority);
        }
        devices
            .try_reserve_exact(report.devices.len())
            .map_err(|_| full(hex_api::MAX_DEVICES))?;
        devices.extend(report.devices);
        for observation in report.unavailable {
            add_unavailable(&mut unavailable, observation)?;
        }
    }
    let Some(authority) = authority else {
        return Ok(None);
    };
    devices.sort_unstable_by_key(|device| device.info.id);
    let mut unique: Vec<PhysicalDevice> = Vec::new();
    unique
        .try_reserve_exact(devices.len().min(hex_api::MAX_DEVICES))
        .map_err(|_| full(hex_api::MAX_DEVICES))?;
    for device in devices {
        if let Some(previous) = unique
            .last_mut()
            .filter(|previous| previous.info.id == device.info.id)
        {
            if previous.info != device.info {
                return Err(invalid("one native identity has contradictory metadata"));
            }
            // Aliases can sample the same provider at different instants. Keep observed pressure;
            // an unknown independent counter cannot acquire knowledge from another alias's sample.
            previous.busy_per_mille = previous
                .busy_per_mille
                .zip(device.busy_per_mille)
                .map(|(a, b)| a.max(b));
            previous.memory_used_bytes = previous
                .memory_used_bytes
                .zip(device.memory_used_bytes)
                .map(|(a, b)| a.max(b));
        } else {
            if unique.len() >= hex_api::MAX_DEVICES {
                return Err(full(hex_api::MAX_DEVICES));
            }
            unique.push(device);
        }
    }
    let report = PhysicalReport {
        authority,
        devices: unique,
        unavailable,
    };
    report.check()?;
    Ok(Some(report))
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_api::{DeviceId, DeviceInfo, DeviceKind};

    fn report(authority: &str, busy: Option<u16>, used: Option<u64>) -> PhysicalReport {
        PhysicalReport {
            authority: authority.into(),
            unavailable: Vec::new(),
            devices: vec![PhysicalDevice {
                info: DeviceInfo {
                    id: DeviceId {
                        kind: DeviceKind::Gpu,
                        key: 7,
                    },
                    model: "native".into(),
                    size: 10,
                    holder: None,
                },
                busy_per_mille: busy,
                memory_used_bytes: used,
            }],
        }
    }

    /// AC-T7.4.1: native aliases share one identity and cannot turn unknown pressure into free room.
    #[test]
    fn native_aliases_keep_one_owner_independent_unknowns_and_observed_pressure() {
        let got = coalesce(vec![
            report("owner", Some(0), Some(11)),
            report("owner", None, Some(12)),
        ])
        .unwrap()
        .unwrap();
        assert_eq!(got.devices.len(), 1);
        assert_eq!(got.devices[0].busy_per_mille, None);
        assert_eq!(got.devices[0].memory_used_bytes, Some(12));
        assert!(got.devices[0].info.holder.is_none());
        assert!(
            coalesce(vec![
                report("a", Some(0), Some(0)),
                report("b", Some(0), Some(0))
            ])
            .is_err()
        );
        let mut conflict = report("owner", Some(0), Some(0));
        conflict.devices[0].info.size = 11;
        assert!(coalesce(vec![report("owner", Some(0), Some(0)), conflict]).is_err());
    }

    /// AC-T7.4.1: an initialized CPU-only owner is distinguishable from no configured backend.
    #[test]
    fn empty_native_inventory_preserves_its_authority() {
        let got = coalesce(vec![PhysicalReport {
            authority: "cpu-owner".into(),
            unavailable: Vec::new(),
            devices: Vec::new(),
        }])
        .unwrap()
        .unwrap();
        assert_eq!(got.authority, "cpu-owner");
        assert!(got.devices.is_empty());
        assert_eq!(coalesce(Vec::new()).unwrap(), None);
    }
}
