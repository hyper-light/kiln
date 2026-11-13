//! Exact renderer/native correspondence. Every read uses Books' same initialized
//! provider; this module stores neither inventory nor holder state.

use super::*;
use crate::devices::{Binding, Route};

fn invalid(why: &'static str) -> Refusal {
    let mut message = String::new();
    if message.try_reserve_exact(why.len()).is_err() {
        return Refusal::Exhausted {
            what: "physical correspondence refusal bytes",
            bound: why.len(),
        };
    }
    message.push_str(why);
    Refusal::Invalid {
        what: "Shards physical GPU correspondence",
        why: message,
    }
}

fn key(physical: shards_api::PhysicalGpu) -> Result<u64, Refusal> {
    match physical {
        shards_api::PhysicalGpu::Pci {
            domain,
            bus,
            device,
            function,
        } => Ok((u64::from(domain) << 16)
            | (u64::from(bus) << 8)
            | (u64::from(device) << 3)
            | u64::from(function)),
        shards_api::PhysicalGpu::MetalRegistry(id) => Ok(id),
        shards_api::PhysicalGpu::Luid(_) => Err(invalid(
            "a current-boot LUID is not a stable physical DeviceId",
        )),
    }
}

pub(super) fn joined_key(device: &shards_api::ResolvedGpu) -> Result<u64, Refusal> {
    match device.physical {
        shards_api::PhysicalGpu::Luid(luid) => {
            // No integer equality between DeviceId and LUID. Without complete
            // Vulkan PCI properties the provider's separate PNP/KMT join is owed.
            if device.luid != Some(luid.to_le_bytes()) {
                return Err(invalid("native LUID bytes changed"));
            }
            let Some(pci @ shards_api::PhysicalGpu::Pci { .. }) = device.pci else {
                return Err(invalid(
                    "live LUID has no verified complete PCI correspondence",
                ));
            };
            key(pci)
        }
        physical => key(physical),
    }
}

pub(super) fn check_report(
    devices: &[hex_api::DeviceId],
    physical: &[shards_api::PhysicalGpu],
    scope: hex_core::record::NativeScope,
    report: &shards_api::physical::GpuReport,
) -> Result<(), Refusal> {
    report
        .check(*scope.bytes(), physical)
        .map_err(|_| invalid("GPU report differs from the original scope or selector set"))?;
    if report.probe.devices.len() != devices.len() {
        return Err(invalid("GPU report differs from the original claim count"));
    }
    for (id, resolved) in devices.iter().zip(&report.probe.devices) {
        let known_key = match resolved.physical {
            shards_api::PhysicalGpu::Luid(luid) if resolved.pci.is_none() => {
                if resolved.luid != Some(luid.to_le_bytes()) {
                    return Err(invalid("report LUID bytes differ from the exact selector"));
                }
                // A durable codec cannot query native current-boot identity.
                // No PCIEXT is fabricated; the initialized SAME Books provider
                // MUST establish stable DeviceId -> live LUID before use.
                None
            }
            _ => Some(joined_key(resolved)?),
        };
        if id.kind != hex_api::DeviceKind::Gpu || known_key.is_some_and(|key| key != id.key) {
            return Err(invalid(
                "GPU report has no exact stable native claim correspondence",
            ));
        }
    }
    Ok(())
}

impl GuestRuntime {
    pub(super) fn check_native_report(
        &self,
        devices: &[hex_api::DeviceId],
        physical: &[shards_api::PhysicalGpu],
        scope: hex_core::record::NativeScope,
        report: &shards_api::physical::GpuReport,
    ) -> Result<(), Refusal> {
        check_report(devices, physical, scope, report)?;
        for (id, resolved) in devices.iter().zip(&report.probe.devices) {
            if matches!(resolved.physical, shards_api::PhysicalGpu::Luid(_)) {
                if resolved.pci.is_none() {
                    // LUID alone does not identify a physical node in an LDA.
                    // The typed native-node witness and renderer node mask are
                    // not yet part of this signed API; do not publish Live from
                    // a merely unique enumeration or matching adapter integer.
                    return Err(invalid(
                        "native physical-node correspondence is not initialized",
                    ));
                }
                let luid = self.authority()?.physical_luid(*id)?;
                if resolved.luid != Some(luid) {
                    return Err(invalid(
                        "same native provider reports another current-boot LUID",
                    ));
                }
            }
        }
        Ok(())
    }

    /// Before native launch, join each claim configured by a current-boot LUID
    /// to the SAME provider's LUID for its stable DeviceId now: Windows assigns
    /// LUIDs per boot, so a stale one can select another adapter. A stale or
    /// unknown identity is the caller's typed `uncertain`.
    pub(super) fn check_current_luids(
        &self,
        devices: &[hex_api::DeviceId],
        physical: &[shards_api::PhysicalGpu],
        uncertain: Refusal,
    ) -> Result<(), Refusal> {
        if devices.len() != physical.len() {
            return Err(invalid("physical claim count changed"));
        }
        for (id, selector) in devices.iter().zip(physical) {
            let shards_api::PhysicalGpu::Luid(configured) = *selector else {
                continue;
            };
            match self.authority()?.physical_luid(*id) {
                Ok(current) if current == configured.to_le_bytes() => {}
                _ => return Err(uncertain),
            }
        }
        Ok(())
    }

    /// A transient sorted view of the one immutable catalog. References provide
    /// lookup only: no copied metadata, inventory cache or selection authority.
    pub(super) fn physical_entries(&self) -> Result<Vec<&crate::devices::Entry>, Refusal> {
        let catalog = self
            .gpu_catalog
            .as_ref()
            .ok_or_else(|| invalid("no exact physical catalog"))?;
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(catalog.entries().len())
            .map_err(|_| Refusal::Full(hex_api::MAX_DEVICES))?;
        entries.extend(catalog.entries_for(Route::ShardsGpu));
        entries.sort_unstable_by_key(|entry| entry.info.id);
        Ok(entries)
    }

    pub(super) fn catalog_physical(
        &self,
        devices: &[hex_api::DeviceId],
    ) -> Result<Vec<shards_api::PhysicalGpu>, Refusal> {
        let entries = self.physical_entries()?;
        let mut physical = Vec::new();
        physical
            .try_reserve_exact(devices.len())
            .map_err(|_| Refusal::Full(hex_api::MAX_DEVICES))?;
        for id in devices {
            let entry = entries
                .binary_search_by_key(id, |entry| entry.info.id)
                .ok()
                .and_then(|index| entries.get(index))
                .ok_or_else(|| invalid("claim is absent from the configured catalog"))?;
            let Some(Binding::ShardsGpu { physical: native }) = entry.binding(Route::ShardsGpu)
            else {
                return Err(invalid("claim has no configured Shards route"));
            };
            physical.push(*native);
        }
        shards_api::physical::check_selection(&physical)
            .map_err(|_| invalid("physical selectors alias or exceed their representation"))?;
        Ok(physical)
    }

    /// Read only facts from the original charged live observer. A file/report
    /// alone is insufficient: the SAME canonical native scope must still exist.
    fn live_gpu_receipt(
        &self,
        ids: &[hex_api::DeviceId],
    ) -> Result<Option<super::pool::Receipt>, Refusal> {
        let authority = self.authority()?;
        let records = authority.records()?;
        let pools = authority.pool_records()?;
        let Some(record) = self.gpu_fresh_candidate(&records, &pools)? else {
            return Ok(None);
        };
        if record.phase != hex_api::BackingPhase::Live || record.devices != ids {
            return Ok(None);
        }
        let definition = self.gpu_definition_for_record(record)?;
        self.preflight_backing(record)?;
        let receipt = super::pool::read(&definition.path)?
            .ok_or_else(|| super::pool::refused(record.key, hex_api::LifecycleCause::Uncertain))?;
        receipt.matches(record)?;
        if !matches!(receipt.phase, selected::Phase::Running(_)) {
            return Err(super::pool::refused(
                record.key,
                hex_api::LifecycleCause::Uncertain,
            ));
        }
        let report = receipt
            .checked_report()?
            .ok_or_else(|| invalid("live GPU backing lacks its scoped renderer report"))?;
        self.check_native_report(
            &receipt.original.devices,
            &receipt.physical,
            receipt.original.scope(),
            report,
        )?;
        receipt.checked_work_ready()?;
        let identity = receipt.identity()?;
        managed::same_native(hex_core::record::NativeRecord::Pool(record), identity)?;
        let books = self
            .books
            .get()
            .ok_or_else(|| super::pool::refused(record.key, hex_api::LifecycleCause::Uncertain))?;
        let keys = self.turn_keys(&record.devices)?;
        crate::devices::with_pool_transaction(books, &record.key, &keys, |transaction| {
            let current = transaction.read_pool(&record.key)?.ok_or_else(|| {
                super::pool::refused(record.key, hex_api::LifecycleCause::Uncertain)
            })?;
            if current.phase != hex_api::BackingPhase::Live
                || current.native_commitment()? != record.native_commitment()?
                || current.init != record.init
            {
                return Err(super::pool::refused(
                    record.key,
                    hex_api::LifecycleCause::Uncertain,
                ));
            }
            let native = transaction.attach_managed(Some(identity))?;
            if native
                .identity()
                .map_err(|why| refusal("observing original GPU backing identity", &why))?
                != identity
                || native
                    .terminal()
                    .map_err(|why| refusal("observing original GPU backing scope", &why))?
                    .is_some()
            {
                return Err(super::pool::refused(
                    record.key,
                    hex_api::LifecycleCause::Uncertain,
                ));
            }
            Ok(())
        })?;
        Ok(Some(receipt))
    }

    pub(super) fn checked_physical(
        &self,
        ids: &[hex_api::DeviceId],
        physical: &[shards_api::PhysicalGpu],
    ) -> Result<Vec<hex_api::DeviceInfo>, Refusal> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        if ids.len() != physical.len() || ids.len() > hex_api::MAX_DEVICES {
            return Err(invalid("physical claim count changed"));
        }
        let Some(receipt) = self.live_gpu_receipt(ids)? else {
            // Prepared/Granted remains explicit in Capacity.backings; there is
            // no observed GPU eligibility before native+guest readiness is Live.
            return Ok(Vec::new());
        };
        let probe = &receipt
            .checked_report()?
            .ok_or_else(|| invalid("live GPU backing has no private renderer facts"))?
            .probe;
        let authority = self.authority()?;
        let records = authority.records()?;
        let pools = authority.pool_records()?;
        let mut native = authority.physical_devices()?;
        if native.len() > hex_api::MAX_DEVICES {
            return Err(Refusal::Full(hex_api::MAX_DEVICES));
        }
        native.sort_unstable_by_key(|info| info.id);
        if native.windows(2).any(|pair| {
            pair.first()
                .zip(pair.get(1))
                .is_some_and(|(a, b)| a.id == b.id)
        }) {
            return Err(invalid("native provider repeated a physical DeviceId"));
        }
        let catalog = self
            .gpu_catalog
            .as_ref()
            .ok_or_else(|| invalid("physical catalog disappeared"))?;
        let mut offers = Vec::new();
        offers
            .try_reserve_exact(ids.len())
            .map_err(|_| Refusal::Full(hex_api::MAX_DEVICES))?;
        if ids
            .windows(2)
            .any(|pair| matches!(pair, [left, right] if left >= right))
        {
            return Err(invalid(
                "physical claims are not canonical sorted unique identities",
            ));
        }
        let mut native = native.into_iter().peekable();
        for (id, selector) in ids.iter().zip(physical) {
            let resolved = probe
                .devices
                .iter()
                .find(|resolved| resolved.physical == *selector)
                .ok_or_else(|| {
                    invalid("live renderer does not contain the complete requested selector")
                })?;
            if id.kind != hex_api::DeviceKind::Gpu
                || (!matches!(resolved.physical, shards_api::PhysicalGpu::Luid(_))
                    && joined_key(resolved)? != id.key)
            {
                return Err(invalid(
                    "complete native identity disagrees with the stable physical claim",
                ));
            }
            while native.peek().is_some_and(|info| info.id < *id) {
                let _ = native.next();
            }
            let mut info = match native.peek().map(|info| info.id) {
                Some(native_id) if native_id == *id => native
                    .next()
                    .ok_or_else(|| invalid("native physical snapshot changed"))?,
                _ => {
                    let (Some(name), Some(size)) = (&resolved.name, resolved.device_local_bytes)
                    else {
                        // Unknown native properties stay unknown, not zero or
                        // configuration metadata. No offer can be published.
                        return Ok(Vec::new());
                    };
                    hex_api::DeviceInfo {
                        id: *id,
                        model: name.try_text().map_err(|why| {
                            refusal(
                                "copying actual renderer device name",
                                &shards_api::Refusal::GpuProbe(why),
                            )
                        })?,
                        size,
                        holder: None,
                    }
                }
            };
            info.check()?;
            if info.holder.is_some() {
                return Err(invalid("native facts cannot assert a holder"));
            }
            // Move actual metadata once, without a fake placeholder DeviceInfo.
            // Only the canonical Box+Pool records supply the physical holder.
            info.holder = catalog.holder(*id, &records, &pools)?;
            offers.push(info);
        }
        Ok(offers)
    }

    /// The existing native owner calls this after releasing its provider lock,
    /// then joins these observations into that SAME authority's physical report.
    /// A cold key supplies only a qualified refusal, never invented metadata.
    pub fn physical_observations(&self) -> Result<Vec<hex_api::PhysicalDevice>, Refusal> {
        use hex_api::{BackingPhase, LifecycleCause};
        if self.gpu_backing_definition()?.is_none() {
            return Ok(Vec::new());
        }
        let authority = self.authority()?;
        let records = authority.records()?;
        let pools = authority.pool_records()?;
        let (current, pending_ack) = self.gpu_family_rows(&records, &pools)?;
        let record = match current {
            Some(record) => record,
            None => {
                let retained = pending_ack.or_else(|| {
                    pools
                        .iter()
                        .filter(|row| row.execution == hex_api::Execution::ShardsDedicated)
                        .min_by_key(|row| row.key)
                });
                return match retained {
                    Some(row) => Err(super::pool::refused(row.key, LifecycleCause::Busy)),
                    None => Ok(Vec::new()),
                };
            }
        };
        if record.phase == BackingPhase::Live
            && self.gpu_fresh_candidate(&records, &pools)?.is_none()
        {
            return Err(super::pool::refused(record.key, LifecycleCause::Busy));
        }
        let definition = self.gpu_definition_for_record(record)?;
        self.preflight_backing(record)?;
        match record.phase {
            BackingPhase::Prepared | BackingPhase::Granted => {
                return Err(super::pool::refused(record.key, LifecycleCause::Pending));
            }
            BackingPhase::Draining | BackingPhase::Released => {
                return Err(super::pool::refused(record.key, LifecycleCause::Busy));
            }
            BackingPhase::Live => {}
        }
        let actual = self.checked_physical(&definition.devices, &definition.physical)?;
        if actual.len() != definition.devices.len() {
            return Err(super::pool::refused(record.key, LifecycleCause::Uncertain));
        }
        let mut observations = Vec::new();
        observations
            .try_reserve_exact(actual.len())
            .map_err(|_| Refusal::Exhausted {
                what: "same-scope Shards physical observations",
                bound: hex_api::MAX_DEVICES,
            })?;
        for mut info in actual {
            // Telemetry never grants holders. Only the canonical Store supplies
            // those; no unobserved pressure or memory-used value becomes zero.
            info.holder = None;
            observations.push(hex_api::PhysicalDevice {
                info,
                busy_per_mille: None,
                memory_used_bytes: None,
            });
        }
        Ok(observations)
    }

    pub fn attachment_offers(&self) -> Result<Vec<hex_api::DeviceInfo>, Refusal> {
        if self.gpu_backing_definition()?.is_none() {
            return Ok(Vec::new());
        }
        let authority = self.authority()?;
        let records = authority.records()?;
        let pools = authority.pool_records()?;
        let Some(record) = self.gpu_fresh_candidate(&records, &pools)? else {
            return Ok(Vec::new());
        };
        if record.phase != hex_api::BackingPhase::Live {
            return Ok(Vec::new());
        }
        let definition = self.gpu_definition_for_record(record)?;
        self.checked_physical(&definition.devices, &definition.physical)
    }

    /// Explicit initialization uses the same production join as later admission.
    pub fn initialize_gpu(&self) -> Result<(), Refusal> {
        if self.gpu_catalog.is_none() {
            return Ok(());
        }
        let expected = self.physical_entries()?.len();
        let actual = self.attachment_offers()?;
        if expected != actual.len() {
            return Err(invalid(
                "configured physical identities have no complete current native/renderer join",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// AC-T7.4.1: current-boot LUID bytes never become a stable integer identity;
    /// missing PCI domain or stale LUID correspondence remains unknown/refused.
    #[test]
    fn luid_requires_complete_current_native_correspondence() {
        let pci = shards_api::PhysicalGpu::Pci {
            domain: 7,
            bus: 2,
            device: 1,
            function: 3,
        };
        let mut resolved = shards_api::ResolvedGpu {
            physical: shards_api::PhysicalGpu::Luid(9),
            uuid: [1; 16],
            pci: None,
            luid: Some(9u64.to_le_bytes()),
            name: None,
            device_local_bytes: None,
        };
        assert!(joined_key(&resolved).is_err());
        resolved.pci = Some(pci);
        assert_eq!(
            joined_key(&resolved).unwrap(),
            (7 << 16) | (2 << 8) | (1 << 3) | 3
        );
        resolved.luid = Some(10u64.to_le_bytes());
        assert!(joined_key(&resolved).is_err());
        assert!(key(shards_api::PhysicalGpu::Luid(9)).is_err());
    }
}
