//! Pool rows use the SAME Store/Turns and durable publication as a scoped box row.

use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use hex_api::{
    BackingGrant, BackingKey, BackingPhase, DeviceId, Execution, LifecycleCause, Refusal,
    RuntimeCost,
};
use hex_core::record::{PoolRecord, RECORD_VERSION};

use super::{Store, TURNS, fresh_scope, owner_only};

fn allocation(bound: usize) -> Refusal {
    Refusal::Exhausted {
        what: "native backing record storage",
        bound,
    }
}
fn invalid(message: &'static str) -> Refusal {
    let mut why = String::new();
    if why.try_reserve_exact(message.len()).is_err() {
        return allocation(message.len());
    }
    why.push_str(message);
    Refusal::Invalid {
        what: "native backing record",
        why,
    }
}
fn refused(key: BackingKey) -> Refusal {
    Refusal::Backing {
        key,
        cause: LifecycleCause::Uncertain,
    }
}

fn join(directory: &Path, name: &str) -> Result<PathBuf, Refusal> {
    let count = directory
        .as_os_str()
        .len()
        .checked_add(name.len())
        .and_then(|count| count.checked_add(1))
        .ok_or_else(|| allocation(usize::MAX))?;
    let mut path = PathBuf::new();
    path.try_reserve_exact(count)
        .map_err(|_| allocation(count))?;
    path.push(directory);
    path.push(name);
    Ok(path)
}

pub(super) fn scoped_paths(directory: &Path, id: &str) -> Result<(PathBuf, PathBuf), Refusal> {
    let room = id
        .len()
        .checked_add(".json".len())
        .ok_or_else(|| allocation(usize::MAX))?;
    let mut name = String::new();
    name.try_reserve_exact(room).map_err(|_| allocation(room))?;
    name.push_str(id);
    name.push_str(".json");
    let final_path = join(directory, &name)?;
    name.clear();
    name.push('.');
    name.push_str(id);
    name.push_str(".tmp");
    Ok((join(directory, &name)?, final_path))
}

fn name(pool: &[u8; 32]) -> Result<String, Refusal> {
    let count = pool
        .len()
        .checked_mul(2)
        .ok_or_else(|| allocation(usize::MAX))?;
    let mut name = String::new();
    name.try_reserve_exact(count)
        .map_err(|_| allocation(count))?;
    for byte in pool {
        for digit in [byte >> 4, byte & 15] {
            let digit = b"0123456789abcdef"
                .get(usize::from(digit))
                .copied()
                .ok_or_else(|| allocation(count))?;
            name.push(char::from(digit));
        }
    }
    Ok(name)
}

fn pool_from_name(name: &std::ffi::OsStr) -> Option<[u8; 32]> {
    let text = name.to_str()?.strip_suffix(".json")?;
    if text.len() != 64
        || !text
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        return None;
    }
    let (pairs, tail) = text.as_bytes().as_chunks::<2>();
    if !tail.is_empty() {
        return None;
    }
    let mut pool = [0; 32];
    for (byte, pair) in pool.iter_mut().zip(pairs) {
        let pair = std::str::from_utf8(pair).ok()?;
        *byte = u8::from_str_radix(pair, 16).ok()?;
    }
    Some(pool)
}

fn read_pool_file(file: File, pool: &[u8; 32]) -> Result<PoolRecord, Refusal> {
    let bytes = super::read_record_bytes(file)?;
    let record = PoolRecord::decode(&bytes)?;
    if record.key.pool != *pool {
        return Err(refused(record.key));
    }
    Ok(record)
}

impl Store {
    fn pool_directory(&self) -> Result<PathBuf, Refusal> {
        join(&self.disk, ".pools")
    }

    /// Prepared initialization is internal to configured backends, never arbitrary peer input.
    /// No native effect or Index grant is permitted until this SAME fresh scope is durable.
    #[cfg(test)]
    pub(crate) fn prepare_pool(
        &self,
        pool: [u8; 32],
        execution: Execution,
        cost: RuntimeCost,
        devices: &[DeviceId],
    ) -> Result<PoolRecord, Refusal> {
        self.prepare_pool_with(pool, execution, cost, devices, fresh_scope)
    }

    #[cfg(test)]
    fn prepare_pool_with(
        &self,
        pool: [u8; 32],
        execution: Execution,
        cost: RuntimeCost,
        devices: &[DeviceId],
        create_scope: impl FnOnce() -> Result<hex_core::record::NativeScope, Refusal>,
    ) -> Result<PoolRecord, Refusal> {
        let _metadata = self.preparing_metadata()?;
        let _turn = self.turns.take_pool_configuration(&pool)?;
        self.prepare_pool_guarded(pool, execution, cost, devices, create_scope)
    }

    /// The SAME preparation fence covers the complete retained snapshot, stale-proposal check,
    /// pure configured validator, and actual publication. The callback cannot reenter this Store.
    pub(crate) fn prepare_pool_validated(
        &self,
        pool: [u8; 32],
        execution: Execution,
        cost: RuntimeCost,
        devices: &[DeviceId],
        family: (&[u8; 32], &[DeviceId], Option<&PoolRecord>),
        validate: &mut hex_runtime::devices::PoolPreparationCheck<'_>,
    ) -> Result<PoolRecord, Refusal> {
        let (configuration, conflicts, expected) = family;
        cost.check()?;
        if devices.len() > hex_api::MAX_DEVICES || conflicts.len() > hex_api::MAX_DEVICES {
            return Err(allocation(hex_api::MAX_DEVICES));
        }
        for ids in [devices, conflicts] {
            for id in ids {
                id.check()?;
            }
            if ids
                .windows(2)
                .any(|pair| matches!(pair, [left, right] if left >= right))
            {
                return Err(invalid("backing preparation selections must be canonical"));
            }
        }
        if devices
            .iter()
            .any(|id| conflicts.binary_search(id).is_err())
            || expected.is_some_and(|row| {
                row.devices
                    .iter()
                    .any(|id| conflicts.binary_search(id).is_err())
            })
        {
            return Err(invalid(
                "backing preparation is missing a full-family conflict key",
            ));
        }
        let _metadata = self.preparing_metadata()?;
        let _turn = self.turns.take_pool_configuration(configuration)?;
        let records = self.retained_records()?;
        let mut pools = self.all_pools()?;
        for (id, row) in &records {
            let row = row.as_ref().map_err(|_| super::uncertain(id))?;
            if row.version != RECORD_VERSION || row.spec.id != *id {
                return Err(super::uncertain(id));
            }
            row.spec.check()?;
        }
        // These snapshots are complete and ordered; the expected row includes generation,
        // phase, native identity and every immutable cost/device commitment, not just its key.
        let actual = pools
            .iter()
            .find(|row| row.key.pool == expected.map_or(pool, |row| row.key.pool));
        if actual != expected {
            return Err(match expected.or(actual) {
                Some(row) => refused(row.key),
                None => invalid("backing preparation lost its expected current row"),
            });
        }
        if let Some(expected) = expected
            && expected.key.pool != pool
            && let Some(conflicting) = pools.iter().find(|row| row.key.pool == pool)
        {
            return Err(refused(conflicting.key));
        }
        validate(&records, &pools)?;
        if let Some(expected) = expected {
            let at = pools
                .binary_search_by_key(&expected.key, |row| row.key)
                .map_err(|_| refused(expected.key))?;
            if at >= pools.len() {
                return Err(refused(expected.key));
            }
            // Move this exact canonical row out of the temporary snapshot. No deep clone.
            let mut current = pools.remove(at);
            if current.execution != execution || current.cost != cost {
                return Err(refused(current.key));
            }
            if current.key.pool == pool {
                if current.devices != devices {
                    return Err(refused(current.key));
                }
                return Ok(current);
            }
            if records.values().any(|row| {
                matches!(row,
                Ok(row) if row.spec.route.and_then(|route| route.backing)
                    .is_some_and(|source| source.pool == current.key.pool))
            }) {
                return Err(Refusal::Backing {
                    key: current.key,
                    cause: LifecycleCause::Busy,
                });
            }
            // A changed complement cannot replace an admitted native scope. Publish only its
            // drain intent, then return the OLD row for native cleanup/Index acknowledgment.
            // Prepared is unchanged: its adapter still owes exact no-submission proof.
            if matches!(current.phase, BackingPhase::Granted | BackingPhase::Live) {
                current.phase = BackingPhase::Draining;
                current.check()?;
                self.write_pool(&current)?;
            }
            return Ok(current);
        }
        if let Some(source) = Self::pool_reference(&records, &pool)? {
            return Err(refused(source));
        }
        if pools.len() >= TURNS {
            return Err(allocation(TURNS));
        }
        self.publish_prepared(pool, execution, cost, devices, fresh_scope)
    }

    /// Called only while this Store's existing metadata preparation fence is exclusive.
    #[cfg(test)]
    fn prepare_pool_guarded(
        &self,
        pool: [u8; 32],
        execution: Execution,
        cost: RuntimeCost,
        devices: &[DeviceId],
        create_scope: impl FnOnce() -> Result<hex_core::record::NativeScope, Refusal>,
    ) -> Result<PoolRecord, Refusal> {
        if let Some(record) = self.read_pool(&pool)? {
            if record.execution != execution || record.cost != cost || record.devices != devices {
                return Err(refused(record.key));
            }
            return Ok(record);
        }
        // A missing old Pool row is not permission to rearm a configuration still named by a Box.
        // Keep that immutable reference for whole-owner recovery; generate no replacement scope.
        if let Some(source) = self.referenced_pool(&pool)? {
            return Err(Refusal::Backing {
                key: source,
                cause: LifecycleCause::Uncertain,
            });
        }
        // Configured native owners share their existing MAX_BOXES slot bound. Scans and source
        // preparation are bounded here too; a grant is still admitted by the sole Index pool.
        if self.all_pools()?.len() >= TURNS {
            return Err(allocation(TURNS));
        }
        self.publish_prepared(pool, execution, cost, devices, create_scope)
    }

    fn publish_prepared(
        &self,
        pool: [u8; 32],
        execution: Execution,
        cost: RuntimeCost,
        devices: &[DeviceId],
        create_scope: impl FnOnce() -> Result<hex_core::record::NativeScope, Refusal>,
    ) -> Result<PoolRecord, Refusal> {
        let mut selected = Vec::new();
        if devices.len() > hex_api::MAX_DEVICES {
            return Err(allocation(hex_api::MAX_DEVICES));
        }
        selected
            .try_reserve_exact(devices.len())
            .map_err(|_| allocation(hex_api::MAX_DEVICES))?;
        selected.extend_from_slice(devices);
        let scope = create_scope()?;
        let record = PoolRecord {
            version: RECORD_VERSION,
            key: BackingKey {
                pool,
                generation: *scope.bytes(),
            },
            execution,
            cost,
            allocation: None,
            devices: selected,
            phase: BackingPhase::Prepared,
            init: None,
            exit: None,
        };
        record.check()?;
        let bytes = record.encode()?;
        let directory = self.pool_directory()?;
        let (temporary, path) = scoped_paths(&directory, &name(&pool)?)?;
        fs::create_dir_all(&directory)
            .and_then(|()| owner_only(&directory))
            .map_err(|error| Refusal::os("creating the native backing record directory", error))?;
        // An exclusive fresh name cannot overwrite an earlier generation, even from another
        // owner process. A crash mid-write retains an unreadable, never-granted row as uncertainty.
        // Successful initialization flushes the complete Prepared row BEFORE any grant/native call.
        let file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| Refusal::os("claiming a fresh native backing record", error))?;
        drop(file);
        self.publish_durable(&directory, &temporary, &path, &bytes)?;
        #[cfg(not(windows))]
        self.sync_dir(&self.disk)?;
        Ok(record)
    }

    pub(crate) fn read_pool(&self, pool: &[u8; 32]) -> Result<Option<PoolRecord>, Refusal> {
        let (_, path) = scoped_paths(&self.pool_directory()?, &name(pool)?)?;
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(Refusal::os("opening a native backing record", error)),
        };
        read_pool_file(file, pool).map(Some)
    }

    /// Same canonical source/generation/profile and sole consuming reference, never a fresh
    /// provider report. Its caller serializes publication on this Store's existing Pool turn.
    pub(crate) fn check_box_backing(
        &self,
        record: &hex_core::record::BoxRecord,
    ) -> Result<Option<BackingPhase>, Refusal> {
        let Some(route) = record.spec.route else {
            return Ok(None);
        };
        let Some(source) = route.backing else {
            return Ok(None);
        };
        let pool = self
            .read_pool(&source.pool)?
            .ok_or_else(|| refused(source))?;
        crate::recorded::check_backing_reference(record, &pool)?;
        if route.key.execution != Execution::ShardsShared {
            if pool.phase == BackingPhase::Draining
                && self
                    .read_lifecycle(&record.spec.id)?
                    .is_none_or(|current| current.spec != record.spec)
            {
                // Coalescing has already excluded new consumers. Only this SAME canonical
                // reference can reenter/cancel while the old full scope is still draining.
                // The writer holds source then Box Store turns across this check/publication.
                return Err(Refusal::Backing {
                    key: source,
                    cause: LifecycleCause::Busy,
                });
            }
            crate::recorded::check_single_consumer(
                &self.retained_records()?,
                source,
                &record.spec.id,
            )?;
        }
        Ok(Some(pool.phase))
    }

    pub(crate) fn grant_pool(&self, grant: &BackingGrant) -> Result<PoolRecord, Refusal> {
        let _metadata = self.changing_metadata()?;
        let _turn = self.turns.take_pool(&grant.key)?;
        let mut record = self
            .read_pool(&grant.key.pool)?
            .ok_or_else(|| refused(grant.key))?;
        record.accept_grant(grant)?;
        self.write_pool(&record)?;
        Ok(record)
    }

    pub(crate) fn put_pool(&self, record: &PoolRecord) -> Result<(), Refusal> {
        let _metadata = self.changing_metadata()?;
        let _turn = self.turns.take_pool(&record.key)?;
        let current = self
            .read_pool(&record.key.pool)?
            .ok_or_else(|| refused(record.key))?;
        current.accepts(record)?;
        self.write_pool(record)
    }

    fn write_pool(&self, record: &PoolRecord) -> Result<(), Refusal> {
        let bytes = record.encode()?;
        let directory = self.pool_directory()?;
        let (temporary, final_path) = scoped_paths(&directory, &name(&record.key.pool)?)?;
        self.publish_durable(&directory, &temporary, &final_path, &bytes)
    }

    /// A strict bounded SAME-Store snapshot, not a reference cache. Unknown/incomplete rows
    /// refuse; no old generation can be erased/rearmed because its source row disappeared.
    fn referenced_pool(&self, pool: &[u8; 32]) -> Result<Option<BackingKey>, Refusal> {
        Self::pool_reference(&self.retained_records()?, pool)
    }

    fn pool_reference(
        records: &hex_runtime::devices::Records,
        pool: &[u8; 32],
    ) -> Result<Option<BackingKey>, Refusal> {
        for (id, row) in records {
            let row = match row {
                Ok(row) => row,
                Err(error) => {
                    let mut why = String::new();
                    why.try_reserve_exact(error.len())
                        .map_err(|_| allocation(error.len()))?;
                    why.push_str(error);
                    return Err(Refusal::Broken(why));
                }
            };
            if row.version != RECORD_VERSION || row.spec.id != *id {
                return Err(invalid(
                    "retained backing reference has inconsistent identity",
                ));
            }
            row.spec.check()?;
            if let Some(source) = row.spec.route.and_then(|route| route.backing)
                && source.pool == *pool
            {
                return Ok(Some(source));
            }
        }
        Ok(None)
    }

    /// Only exact Released state permits removal; a reused configuration's generation is untouched.
    pub(crate) fn remove_pool(&self, key: &BackingKey) -> Result<(), Refusal> {
        let _metadata = self.changing_metadata()?;
        let _turn = self.turns.take_pool(key)?;
        let Some(record) = self.read_pool(&key.pool)? else {
            return Ok(());
        };
        if record.key != *key || record.phase != BackingPhase::Released {
            return Err(refused(*key));
        }
        // Hold the SAME source metadata turn through the strict scan and unlink. Every reference
        // publication takes it before its Box turn; a new reference cannot appear after this scan.
        if self.referenced_pool(&key.pool)?.is_some() {
            // Includes stale/conflicting generations: preserve their only source metadata for the
            // global barrier instead of overwriting the evidence of an inconsistent join.
            return Err(Refusal::Backing {
                key: *key,
                cause: LifecycleCause::Busy,
            });
        }
        let directory = self.pool_directory()?;
        let (_, path) = scoped_paths(&directory, &name(&key.pool)?)?;
        fs::remove_file(path)
            .map_err(|error| Refusal::os("removing the released native backing record", error))?;
        self.flush_pool_directory(&directory)
    }

    pub(crate) fn all_pools(&self) -> Result<Vec<PoolRecord>, Refusal> {
        let directory = self.pool_directory()?;
        let entries = match fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(Refusal::os("listing native backing records", error)),
        };
        let mut records = Vec::new();
        // At most configured slots plus first publications already admitted by the same bounded
        // Turns can race the count, and each may retain one atomic-publication temporary file.
        // This is a scan budget, not another native/resource grant allowance.
        let bound = TURNS.checked_mul(3).ok_or_else(|| allocation(usize::MAX))?;
        let mut seen = 0usize;
        for entry in entries {
            if seen >= bound {
                return Err(allocation(bound));
            }
            seen = seen.checked_add(1).ok_or_else(|| allocation(bound))?;
            let entry =
                entry.map_err(|error| Refusal::os("listing a native backing record", error))?;
            let filename = entry.file_name();
            let Some(pool) = pool_from_name(&filename) else {
                if super::retained_json_name(&filename) {
                    return Err(invalid(
                        "retained Pool record has no valid canonical configuration name",
                    ));
                }
                continue;
            };
            // Open the actual enumerated spelling, never a reconstructed name that might resolve
            // to a different row (or no row) on a case-sensitive filesystem. Vanishing entries are
            // a refusal during startup, not an absent owner. Interactive read_pool keeps its API.
            let path = join(
                &directory,
                filename
                    .to_str()
                    .ok_or_else(|| invalid("a canonical backing filename is not ASCII"))?,
            )?;
            let file = File::open(path)
                .map_err(|error| Refusal::os("opening a retained native backing record", error))?;
            let record = read_pool_file(file, &pool)?;
            records.try_reserve(1).map_err(|_| allocation(bound))?;
            records.push(record);
        }
        records.sort_unstable_by_key(|record| record.key);
        if records
            .windows(2)
            .any(|pair| matches!(pair, [left, right] if left.key.pool == right.key.pool))
        {
            return Err(invalid("duplicate native backing configuration identity"));
        }
        Ok(records)
    }

    /// Shared scoped Box/Pool publication. The caller has encoded/reserved all bytes and paths.
    pub(super) fn publish_durable(
        &self,
        directory: &Path,
        temporary: &Path,
        final_path: &Path,
        bytes: &[u8],
    ) -> Result<(), Refusal> {
        let mut file = File::create(temporary)
            .map_err(|error| Refusal::os("creating a scoped record temporary file", error))?;
        file.write_all(bytes)
            .and_then(|()| file.sync_all())
            .map_err(|error| Refusal::os("flushing a scoped record", error))?;
        self.synced.files.fetch_add(1, Ordering::Relaxed);
        drop(file);
        #[cfg(windows)]
        hex_runtime::windows::sys::replace_durable(temporary, final_path).map_err(|error| {
            Refusal::os("publishing the scoped record with write-through", error)
        })?;
        #[cfg(not(windows))]
        fs::rename(temporary, final_path)
            .map_err(|error| Refusal::os("publishing a scoped record", error))?;
        self.flush_pool_directory(directory)
    }

    fn flush_pool_directory(&self, directory: &Path) -> Result<(), Refusal> {
        #[cfg(windows)]
        {
            let _ = directory;
        }
        #[cfg(not(windows))]
        {
            File::open(directory)
                .and_then(|directory| directory.sync_all())
                .map_err(|error| Refusal::os("flushing the scoped record directory", error))?;
            self.synced.dirs.fetch_add(1, Ordering::Relaxed);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_core::record::{NativeScope, ProcId};

    pub(super) fn cost() -> RuntimeCost {
        let fixed = hex_api::CapacityResources {
            cpu_milli: 1000,
            mem_bytes: hex_api::MIN_MEMORY,
            disk_bytes: 0,
        };
        RuntimeCost {
            id: [3; 32],
            floor: fixed,
            ceiling: fixed,
            quantum: hex_api::CapacityResources {
                cpu_milli: 1,
                mem_bytes: 1,
                disk_bytes: 1,
            },
            guest_overhead: hex_api::CapacityResources::default(),
            overhead: hex_api::CapacityResources::default(),
        }
    }
    fn prepared(store: &Store, generation: [u8; 16]) -> PoolRecord {
        store
            .prepare_pool_with([1; 32], Execution::ShardsShared, cost(), &[], || {
                Ok(NativeScope::from_bytes(generation))
            })
            .unwrap()
    }

    /// AC-T7.4.1: prepared scope is durable before grant and retry cannot generate a new lifetime.
    #[test]
    fn preparation_retry_keeps_the_original_durable_scope() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("disk"), &dir.path().join("run")).unwrap();
        let first = prepared(&store, [2; 16]);
        let fills = std::cell::Cell::new(0);
        let again = store
            .prepare_pool_with([1; 32], Execution::ShardsShared, cost(), &[], || {
                fills.set(fills.get() + 1);
                Ok(NativeScope::from_bytes([9; 16]))
            })
            .unwrap();
        assert_eq!(fills.get(), 0);
        assert_eq!(again, first);
        assert_eq!(store.read_pool(&[1; 32]).unwrap(), Some(first));
        assert_eq!(store.all_pools().unwrap().len(), 1);
        assert!(store.syncs().files > 0);
    }

    /// AC-T7.4.1: old same-pool cleanup cannot remove a replacement generation or conflicting init.
    #[test]
    fn replacement_generation_refuses_old_grant_and_cleanup() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("disk"), &dir.path().join("run")).unwrap();
        let first = prepared(&store, [2; 16]);
        let grant = BackingGrant {
            key: first.key,
            allocation: first
                .cost
                .allocate(hex_api::CapacityResources::default())
                .unwrap(),
            devices: Vec::new(),
        };
        let mut active = store.grant_pool(&grant).unwrap();
        active.init = Some(ProcId {
            pid: 7,
            start_ticks: 11,
        });
        store.put_pool(&active).unwrap();
        active.phase = BackingPhase::Draining;
        active.exit = Some(hex_api::Exit::Code(7));
        store.put_pool(&active).unwrap();
        active.phase = BackingPhase::Released;
        store.put_pool(&active).unwrap();
        store.remove_pool(&active.key).unwrap();
        let replacement = prepared(&store, [9; 16]);
        assert!(store.grant_pool(&grant).is_err());
        assert!(store.remove_pool(&first.key).is_err());
        assert_eq!(store.read_pool(&[1; 32]).unwrap(), Some(replacement));
    }

    /// T-T7.1.2: native filename aliases are never treated as an absent retained owner.
    /// The `.JSON` case is a portability requirement, not a reproduced Windows filesystem bug.
    #[test]
    fn retained_noncanonical_json_spellings_refuse_and_keep_exact_files() {
        let dir = tempfile::tempdir().unwrap();
        let disk = dir.path().join("disk");
        let store = Store::open(&disk, &dir.path().join("run")).unwrap();
        let canonical = prepared(&store, [2; 16]);
        assert_eq!(store.all_pools().unwrap(), vec![canonical]);
        for filename in [
            format!("{}.json", "A1".repeat(32)),
            format!("{}.JSON", "a2".repeat(32)),
            "@unknown.JsOn".to_owned(),
        ] {
            let path = disk.join(".pools").join(filename);
            let bytes = b"{retained unreadable alias\n";
            std::fs::write(&path, bytes).unwrap();
            assert!(store.all_pools().is_err());
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            std::fs::remove_file(&path).unwrap();
        }
        let path = disk.join("retained-box.JSON");
        let bytes = b"{retained box with noncanonical extension\n";
        std::fs::write(&path, bytes).unwrap();
        assert!(store.retained_records().is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert!(
            store.all().unwrap().is_empty(),
            "legacy ordinary scanner is unchanged"
        );
    }
}

#[cfg(test)]
mod retained_reference;

#[cfg(test)]
mod family_preparation;
