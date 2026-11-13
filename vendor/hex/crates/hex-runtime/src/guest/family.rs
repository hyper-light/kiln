//! Metadata-only reconciliation of one configured GPU family. The canonical
//! Store owns references/scopes/grants; this transient proposal grants nothing.

use hex_api::{BackingOffer, BackingPhase, Execution, LifecycleCause, Refusal};
use hex_core::record::{BoxRecord, PoolRecord};

use super::pool::refused;
use super::shared::GpuBacking;
use super::{GuestRuntime, route};
use crate::devices::Records;

pub(super) enum Proposal<'a> {
    Empty,
    AwaitAck(&'a PoolRecord),
    Prepare {
        pool: [u8; 32],
        devices: Vec<hex_api::DeviceId>,
        current: Option<&'a PoolRecord>,
    },
    Drain {
        current: &'a PoolRecord,
        pool: [u8; 32],
    },
}

impl GuestRuntime {
    /// The owner supplies complete bounded snapshots while holding its SAME
    /// family/configuration and full physical-conflict guard. No I/O or reentry.
    pub(super) fn gpu_proposal<'a>(
        &self,
        family: &GpuBacking,
        records: &Records,
        pools: &'a [PoolRecord],
    ) -> Result<Proposal<'a>, Refusal> {
        let (current, pending_ack) = self.gpu_family_rows(records, pools)?;
        let catalog = self
            .gpu_catalog
            .as_ref()
            .ok_or_else(|| route::invalid("GPU preparation lost its initialized catalog"))?;
        let remaining =
            catalog.unheld_subset(&family.devices, current.map(|row| row.key), records, pools)?;
        if remaining.is_empty() {
            if let Some(row) = pending_ack {
                return Ok(Proposal::AwaitAck(row));
            }
            if let Some(current) = current {
                return Ok(Proposal::Drain {
                    current,
                    pool: self.gpu_backing_pool(family.cost, &[], &[])?,
                });
            }
            return Ok(Proposal::Empty);
        }
        let unchanged = current.is_some_and(|row| row.devices == remaining);
        if !unchanged && let Some(row) = pending_ack {
            return Ok(Proposal::AwaitAck(row));
        }
        // Preserve selector/configuration/path checks before publication; the
        // proposal retains only their derived key and exact device set.
        let definition = self
            .gpu_backing_definition_for(Some(&remaining))?
            .ok_or_else(|| route::invalid("GPU complement has no initialized configuration"))?;
        Ok(Proposal::Prepare {
            pool: definition.pool,
            devices: definition.devices,
            current,
        })
    }

    pub(super) fn gpu_family_rows<'a>(
        &self,
        records: &Records,
        pools: &'a [PoolRecord],
    ) -> Result<(Option<&'a PoolRecord>, Option<&'a PoolRecord>), Refusal> {
        if records.len() > crate::MAX_BOXES || pools.len() > crate::MAX_BOXES {
            return Err(Refusal::Full(crate::MAX_BOXES));
        }
        let mut sorted = Vec::new();
        sorted
            .try_reserve_exact(pools.len())
            .map_err(|_| Refusal::Full(crate::MAX_BOXES))?;
        for row in pools {
            row.check()?;
            sorted.push(row);
        }
        sorted.sort_unstable_by_key(|row| row.key);
        if sorted
            .windows(2)
            .any(|pair| matches!(pair, [left, right] if left.key.pool == right.key.pool))
        {
            return Err(route::invalid(
                "GPU preparation found competing pool generations",
            ));
        }
        let mut references = Vec::new();
        references
            .try_reserve_exact(records.len())
            .map_err(|_| Refusal::Full(crate::MAX_BOXES))?;
        for (id, boxed) in records {
            let boxed = match boxed {
                Ok(boxed) => boxed,
                Err(_) => return Err(super::books::uncertain(id)?),
            };
            if boxed.spec.id != *id {
                return Err(super::books::uncertain(id)?);
            }
            boxed.spec.check()?;
            let Some(receipt) = boxed.spec.route else {
                continue;
            };
            if receipt.key.execution != Execution::ShardsDedicated {
                continue;
            }
            let Some(key) = receipt.backing else { continue };
            let source = sorted
                .binary_search_by_key(&key, |row| row.key)
                .ok()
                .and_then(|at| sorted.get(at))
                .copied()
                .ok_or_else(|| refused(key, LifecycleCause::Uncertain))?;
            self.backing_configuration(source)?;
            receipt.check_backing_parts(
                boxed.spec.allocation.map(|allocation| allocation.cost),
                key,
                source.cost,
                &source.devices,
                &boxed.spec.devices,
            )?;
            check_pending_reference(boxed, source)?;
            references.push(key);
        }
        references.sort_unstable();
        let mut current = None;
        let mut pending_ack = None;
        for row in sorted {
            if row.execution != Execution::ShardsDedicated {
                continue;
            }
            self.backing_configuration(row)?;
            let begin = references.partition_point(|key| *key < row.key);
            let end = references.partition_point(|key| *key <= row.key);
            let count = end
                .checked_sub(begin)
                .ok_or_else(|| refused(row.key, LifecycleCause::Uncertain))?;
            if count > 1 {
                return Err(refused(row.key, LifecycleCause::Busy));
            }
            if count != 0 {
                continue;
            }
            if row.phase == BackingPhase::Released {
                // Sorted canonical keys make this ACK queue deterministic.
                if pending_ack.is_none() {
                    pending_ack = Some(row);
                }
            } else if current.replace(row).is_some() {
                return Err(refused(row.key, LifecycleCause::Uncertain));
            }
        }
        Ok((current, pending_ack))
    }

    /// Geometry alone is not readiness. Fresh admission additionally needs
    /// this exact current set to remain the guarded reconciliation target.
    pub(super) fn gpu_fresh_candidate<'a>(
        &self,
        records: &Records,
        pools: &'a [PoolRecord],
    ) -> Result<Option<&'a PoolRecord>, Refusal> {
        let Some(family) = self.gpu_backing_definition()? else {
            return Ok(None);
        };
        match self.gpu_proposal(&family, records, pools)? {
            Proposal::Prepare {
                pool,
                devices,
                current: Some(row),
            } if row.phase == BackingPhase::Live
                && row.key.pool == pool
                && row.devices == devices =>
            {
                Ok(Some(row))
            }
            _ => Ok(None),
        }
    }

    pub(super) fn prepare_gpu_family(&self) -> Result<Option<BackingOffer>, Refusal> {
        let Some(family) = self.gpu_backing_definition()? else {
            return Ok(None);
        };
        let authority = self.authority()?;
        let records = authority.records()?;
        let pools = authority.pool_records()?;
        // All retained paths/configurations are checked before reconciliation
        // can change even one canonical metadata row. Native effects stay outside.
        for row in &pools {
            if row.execution == Execution::ShardsDedicated {
                self.preflight_backing(row)?;
            }
        }
        let proposal = self.gpu_proposal(&family, &records, &pools)?;
        let (pool, devices, current) = match &proposal {
            Proposal::Empty => return Ok(None),
            Proposal::AwaitAck(row) => return row.offer().map(Some),
            Proposal::Prepare {
                pool,
                devices,
                current,
            } => (*pool, devices.as_slice(), *current),
            Proposal::Drain { current, pool } => (*pool, &[][..], Some(*current)),
        };
        let keys = self.turn_keys(&family.devices)?;
        let mut validate = |records: &Records, pools: &[PoolRecord]| {
            let actual = self.gpu_proposal(&family, records, pools)?;
            let matches = match (&proposal, actual) {
                (
                    Proposal::Prepare {
                        pool,
                        devices,
                        current,
                    },
                    Proposal::Prepare {
                        pool: actual_pool,
                        devices: actual_devices,
                        current: expected,
                    },
                ) => expected == *current && actual_pool == *pool && actual_devices == *devices,
                (
                    Proposal::Drain { current, pool },
                    Proposal::Drain {
                        current: expected,
                        pool: actual,
                    },
                ) => *current == expected && *pool == actual,
                _ => false,
            };
            if matches {
                Ok(())
            } else {
                Err(match current {
                    Some(row) => refused(row.key, LifecycleCause::Uncertain),
                    None => route::invalid("GPU complement changed before guarded publication"),
                })
            }
        };
        let row = authority.prepare_pool(
            pool,
            Execution::ShardsDedicated,
            family.cost,
            devices,
            (&family.pool, &keys, current),
            &mut validate,
        )?;
        self.preflight_backing(&row)?;
        if row.key.pool != pool {
            let original = current.ok_or_else(|| refused(row.key, LifecycleCause::Uncertain))?;
            original.accepts(&row)?;
            if row.phase == BackingPhase::Prepared {
                // Changed uncharged Prepared cannot be booted as the new set.
                // The adapter must prove no local submission before cancellation;
                // existing Index Released-ACK still precedes replacement.
                return self.release_gpu_backing(row.key).map(Some);
            }
        }
        row.offer().map(Some)
    }
}

fn check_pending_reference(boxed: &BoxRecord, source: &PoolRecord) -> Result<(), Refusal> {
    match source.phase {
        BackingPhase::Live | BackingPhase::Draining
            if matches!(
                boxed.phase,
                hex_core::record::Phase::Creating | hex_core::record::Phase::Deleting
            ) && boxed.init.is_none()
                && boxed.run.is_none()
                && boxed.exit.is_none() =>
        {
            Ok(())
        }
        BackingPhase::Released => Ok(()),
        _ => Err(refused(source.key, LifecycleCause::Uncertain)),
    }
}
