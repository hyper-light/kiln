//! Read-only whole-owner native metadata barrier. No adoption, watch, cleanup or publication.

use super::*;
use hex_core::record::NativeStage;
use hex_runtime::devices::managed_native_scope_id;
use hex_runtime::linux::cgroup::BoxCgroup;
use rustix::fs::{AtFlags, FileType, Mode, OFlags, RawDir};
use std::fmt::Write as _;

struct Expected<'a> {
    record: NativeRecord<'a>,
    name: BoxId,
    bounds: Option<&'a hex_api::Bounds>,
}

impl Supervisor {
    /// Called under this canonical Pool's existing method turn. No native slot or named scope
    /// may exist for an ungranted lifetime: Prepared before cancellation, or exact unallocated
    /// Released during explicit Index ACK. Absence is not a released-scope receipt.
    pub(crate) fn check_unsubmitted_pool(
        &self,
        record: NativeRecord<'_>,
        root: &Path,
    ) -> Result<(), Refusal> {
        let present = self.inits()?.get(record.owner()).is_some();
        let name = unsubmitted_pool_name(record, present)?;
        // The table guard ended before native metadata I/O. Same-Pool launch/publication is
        // excluded by the caller's turn; unrelated native owners remain independent.
        check_unsubmitted_pool_path(root, name.as_str(), record)
    }

    pub(crate) fn preflight_managed(
        &self,
        root: &Path,
        retained: &[(NativeRecord<'_>, Option<hex_api::Bounds>)],
    ) -> Result<(), Refusal> {
        let expected = expected(retained)?;
        let mut by_owner = Vec::new();
        by_owner
            .try_reserve_exact(expected.len())
            .map_err(|_| managed_storage())?;
        by_owner.extend(expected.iter());
        by_owner.sort_unstable_by(|a, b| a.record.owner().cmp(&b.record.owner()));
        // The only snapshot copies are the SAME fallible native owners. No native or Store I/O
        // takes place under the init table mutex, and no receipt/reaper/table is created here.
        let natives = {
            let inits = self.inits()?;
            let mut snapshot = Vec::new();
            snapshot
                .try_reserve_exact(inits.rows.len())
                .map_err(|_| managed_storage())?;
            for (owner, init) in &inits.rows {
                if let Init::Managed(native) = init {
                    if owner.as_ref() != native.owner.as_ref() {
                        return Err(managed_table_invalid());
                    }
                    snapshot.push(triomphe::Arc::clone(native));
                }
            }
            snapshot
        };
        for native in &natives {
            let position = by_owner
                .binary_search_by(|entry| entry.record.owner().cmp(&native.owner.as_ref()))
                .map_err(|_| native.uncertain())?;
            let record = by_owner
                .get(position)
                .ok_or_else(|| native.uncertain())?
                .record;
            native.matches_record(record)?;
            if native.identity.get().is_some_and(|identity| {
                record.init()
                    != Some(ProcId {
                        pid: identity.pid,
                        start_ticks: identity.started,
                    })
            }) {
                return Err(native.uncertain());
            }
        }
        preflight(root, &expected)
    }
}

fn unsubmitted_pool_name(record: NativeRecord<'_>, present: bool) -> Result<BoxId, Refusal> {
    let refused = || record.refusal(hex_api::LifecycleCause::Uncertain);
    let NativeRecord::Pool(pool) = record else {
        return Err(refused());
    };
    pool.check()?;
    if present
        || !matches!(
            pool.phase,
            hex_api::BackingPhase::Prepared | hex_api::BackingPhase::Released
        )
        || pool.allocation.is_some()
        || pool.init.is_some()
        || pool.exit.is_some()
    {
        return Err(refused());
    }
    managed_native_scope_id(record)
}

fn check_unsubmitted_pool_path(
    root: &Path,
    name: &str,
    record: NativeRecord<'_>,
) -> Result<(), Refusal> {
    let refused = || record.refusal(hex_api::LifecycleCause::Uncertain);
    // Require the initialized owner root itself. NOENT on an intermediate/missing root is not
    // evidence for this exact scope, and neither a symlink nor an empty preexisting scope suffices.
    let root = rustix::fs::open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| refused())?;
    match rustix::fs::statat(&root, name, AtFlags::SYMLINK_NOFOLLOW) {
        Err(Errno::NOENT) => Ok(()),
        Ok(_) | Err(_) => Err(refused()),
    }
}

fn expected<'a>(
    retained: &'a [(NativeRecord<'a>, Option<hex_api::Bounds>)],
) -> Result<Vec<Expected<'a>>, Refusal> {
    if retained.len() > hex_runtime::MAX_BOXES {
        return Err(managed_storage());
    }
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(retained.len())
        .map_err(|_| managed_storage())?;
    for (record, bounds) in retained {
        if bounds.is_some() != record.allocation().is_some() {
            return Err(record.refusal(hex_api::LifecycleCause::Uncertain));
        }
        if let Some(bounds) = bounds {
            bounds.check()?;
            record.commitment()?;
        }
        entries.push(Expected {
            record: *record,
            name: managed_native_scope_id(*record)?,
            bounds: bounds.as_ref(),
        });
    }
    entries.sort_unstable_by(|a, b| a.name.cmp(&b.name));
    if entries
        .windows(2)
        .any(|pair| matches!(pair, [a,b] if a.name == b.name))
    {
        return Err(managed_table_invalid());
    }
    Ok(entries)
}

fn preflight(root: &Path, expected: &[Expected<'_>]) -> Result<(), Refusal> {
    let root_fd = match rustix::fs::open(
        root,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => Some(fd),
        Err(Errno::NOENT) => None,
        Err(error) => return Err(Refusal::os("observing managed native metadata root", error)),
    };
    if let Some(root_fd) = &root_fd {
        check_names(root_fd, expected)?;
    }
    for entry in expected {
        let path = joined(root, entry.name.as_str())?;
        let Some(cgroup) = BoxCgroup::observe_managed(path)? else {
            // A missing path permits later recovery to observe terminal absence. This barrier
            // itself neither removes a record nor releases its reservation or physical claims.
            if let Some(root_fd) = &root_fd {
                match rustix::fs::statat(root_fd, entry.name.as_str(), AtFlags::SYMLINK_NOFOLLOW) {
                    Err(Errno::NOENT) => {}
                    Ok(_) | Err(_) => {
                        return Err(entry.record.refusal(hex_api::LifecycleCause::Uncertain));
                    }
                }
            }
            check_members(entry.record, false, &[], process_stat)?;
            continue;
        };
        let refused = || entry.record.refusal(hex_api::LifecycleCause::Uncertain);
        let bounds = entry.bounds.ok_or_else(refused)?;
        check_scope(entry.record, bounds, &cgroup)?;
        // Validate pathname still names the same pinned object before ending this observation.
        let pinned = rustix::fs::fstat(cgroup.open_dir()?).map_err(|_| refused())?;
        let named = rustix::fs::statat(
            root_fd.as_ref().ok_or_else(refused)?,
            entry.name.as_str(),
            AtFlags::SYMLINK_NOFOLLOW,
        )
        .map_err(|_| refused())?;
        if pinned.st_dev != named.st_dev || pinned.st_ino != named.st_ino {
            return Err(refused());
        }
    }
    Ok(())
}

fn check_scope(
    record: NativeRecord<'_>,
    bounds: &hex_api::Bounds,
    cgroup: &BoxCgroup,
) -> Result<(), Refusal> {
    let refused = || record.refusal(hex_api::LifecycleCause::Uncertain);
    cgroup.check_managed_bounds(bounds).map_err(|_| refused())?;
    let populated = cgroup.managed_populated().map_err(|_| refused())?;
    let members = cgroup.managed_procs().map_err(|_| refused())?;
    check_leaf(&cgroup.open_dir()?)?;
    check_members(record, populated, &members, process_stat)
}

fn check_names(root: &OwnedFd, expected: &[Expected<'_>]) -> Result<(), Refusal> {
    directories(root, |name| {
        expected
            .binary_search_by(|entry| entry.name.as_str().cmp(name))
            .is_ok()
    })
}

fn check_leaf(scope: &OwnedFd) -> Result<(), Refusal> {
    // Helpers inherit this exact scope. Refuse a delegated child cgroup rather than trust an
    // incomplete root PID list. The existing launcher grants no child-cgroup creation authority.
    directories(scope, |_| false)
}

fn directories(root: &OwnedFd, mut expected: impl FnMut(&str) -> bool) -> Result<(), Refusal> {
    // Linux getdents64 rejects names >=PATH_MAX4096. Two u64s +u16 record length +u8 type,
    // terminating byte and alignment room bound a whole record, even with hostile metadata.
    const ENTRY_BYTES: usize =
        4096 + 2 * size_of::<u64>() + size_of::<u16>() + size_of::<u8>() + align_of::<u64>();
    let mut bytes = [std::mem::MaybeUninit::uninit(); ENTRY_BYTES];
    // A fresh open file description starts at offset zero without changing a shared pinned
    // owner's directory offset, so repeated observations inspect the complete names again.
    let listing = rustix::fs::openat(
        root,
        ".",
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|error| Refusal::os("opening a readonly native metadata listing", error))?;
    let mut dir = RawDir::new(listing, &mut bytes);
    // Separate bounded space for canonical scopes and kernel interface files, plus dot entries;
    // a full MAX_BOXES owner set must not lose two names merely because getdents includes dots.
    let budget = hex_runtime::MAX_BOXES
        .checked_mul(2)
        .and_then(|n| n.checked_add(2))
        .ok_or_else(managed_storage)?;
    for _ in 0..budget {
        let Some(entry) = dir.next() else {
            return Ok(());
        };
        let entry = entry.map_err(|e| Refusal::os("reading managed native metadata names", e))?;
        let name = entry.file_name().to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        match entry.file_type() {
            FileType::RegularFile => {} // Interface files never introduce a native owner.
            FileType::Directory => {
                let name = std::str::from_utf8(name).map_err(|_| managed_table_invalid())?;
                if !expected(name) {
                    return Err(managed_table_invalid());
                }
            }
            _ => return Err(managed_table_invalid()),
        }
    }
    Err(managed_storage())
}

fn check_members(
    record: NativeRecord<'_>,
    populated: bool,
    members: &[u32],
    mut stat: impl FnMut(u32) -> Result<Option<(Stat, u32)>, Refusal>,
) -> Result<(), Refusal> {
    let refused = || record.refusal(hex_api::LifecycleCause::Uncertain);
    // A descendant may remain after the controller exits; cgroup.events is hierarchical.
    // Controller absence alone never certifies that scope is empty or cleanup may proceed.
    if record.stage() == NativeStage::Released && (populated || !members.is_empty()) {
        return Err(refused());
    }
    if !populated && !members.is_empty() {
        return Err(refused());
    }
    let Some(init) = record.init() else {
        return if populated || !members.is_empty() {
            Err(refused())
        } else {
            Ok(())
        };
    };
    for member in members {
        let (actual, policy) = stat(*member)?.ok_or_else(refused)?;
        if !matches!(policy, 0 | 3 | 5)
            || (*member == init.pid && actual.start_ticks != init.start_ticks)
        {
            return Err(refused());
        }
    }
    match stat(init.pid)? {
        Some((actual, policy)) if actual.start_ticks == init.start_ticks => {
            // cpu.max covers fair-class tasks, not realtime/deadline controllers. Kernel
            // SCHED_NORMAL0/BATCH3/IDLE5 are the observed fair-class policy values.
            let ended = actual.zombie && !populated && members.is_empty();
            if !matches!(policy, 0 | 3 | 5) || (!ended && members.binary_search(&init.pid).is_err())
            {
                return Err(refused());
            }
        }
        Some(_) | None if populated || !members.is_empty() => return Err(refused()),
        Some(_) | None => {}
    }
    Ok(())
}

fn process_stat(pid: u32) -> Result<Option<(Stat, u32)>, Refusal> {
    // Reserve the fixed spelling plus the entire u32 decimal width before formatting.
    let width = usize::try_from(u32::MAX.ilog10())
        .ok()
        .and_then(|digits| digits.checked_add(1))
        .and_then(|digits| "/proc//stat".len().checked_add(digits))
        .ok_or_else(managed_storage)?;
    let mut path = String::new();
    path.try_reserve_exact(width)
        .map_err(|_| managed_storage())?;
    write!(path, "/proc/{pid}/stat").map_err(|_| managed_storage())?;
    let fd = match rustix::fs::open(
        path.as_str(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(Errno::NOENT | Errno::SRCH) => return Ok(None),
        Err(error) => {
            return Err(Refusal::os(
                "observing retained managed controller stat",
                error,
            ));
        }
    };
    // Linux v6.19 do_task_stat emits52fields: each numeric value fits a signed/u64 decimal
    // (21 bytes with separator), command proc_task_name[64] and fixed punctuation. One extra byte
    // detects any larger/different kernel layout; no unbounded allocation/read is used.
    const STAT_BYTES: usize = 52 * 21 + 64 + " () \n".len() + 1;
    let mut bytes = [0u8; STAT_BYTES];
    let count = rustix::io::pread(&fd, &mut bytes, 0)
        .map_err(|e| Refusal::os("reading managed controller stat", e))?;
    if count == bytes.len() {
        return Err(managed_table_invalid());
    }
    parse_stat(bytes.get(..count).ok_or_else(managed_table_invalid)?, pid).map(Some)
}

fn parse_stat(bytes: &[u8], pid: u32) -> Result<(Stat, u32), Refusal> {
    let prefix = bytes
        .windows(2)
        .position(|pair| pair == b" (")
        .ok_or_else(managed_table_invalid)?;
    let actual_pid = std::str::from_utf8(bytes.get(..prefix).ok_or_else(managed_table_invalid)?)
        .ok()
        .and_then(|value| value.parse::<u32>().ok());
    if !bytes.ends_with(b"\n") || actual_pid != Some(pid) {
        return Err(managed_table_invalid());
    }
    // The task comm is arbitrary native bytes, and can contain spaces, parentheses or an
    // incomplete UTF8 sequence. Only the numeric/state suffix after its last ')' is ASCII text.
    let end = bytes
        .iter()
        .rposition(|byte| *byte == b')')
        .ok_or_else(managed_table_invalid)?;
    let text = std::str::from_utf8(bytes.get(end..).ok_or_else(managed_table_invalid)?)
        .map_err(|_| managed_table_invalid())?;
    let stat = Stat::parse(text)
        .filter(|stat| stat.start_ticks > 0)
        .ok_or_else(managed_table_invalid)?;
    let (_, rest) = text.rsplit_once(')').ok_or_else(managed_table_invalid)?;
    let policy = rest
        .split_whitespace()
        .nth(38)
        .and_then(|field| field.parse().ok())
        .ok_or_else(managed_table_invalid)?;
    Ok((stat, policy))
}

fn joined(root: &Path, child: &str) -> Result<PathBuf, Refusal> {
    let count = root
        .as_os_str()
        .len()
        .checked_add(child.len())
        .and_then(|n| n.checked_add(1))
        .ok_or_else(managed_storage)?;
    let mut path = PathBuf::new();
    path.try_reserve_exact(count)
        .map_err(|_| managed_storage())?;
    path.push(root);
    path.push(child);
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use hex_api::{BackingKey, BackingPhase, CapacityResources, Execution, RuntimeCost};
    use hex_core::record::{PoolRecord, RECORD_VERSION};

    fn pool() -> PoolRecord {
        let fixed = CapacityResources {
            cpu_milli: 1000,
            mem_bytes: hex_api::MIN_MEMORY,
            disk_bytes: 0,
        };
        let cost = RuntimeCost {
            id: [3; 32],
            floor: fixed,
            ceiling: fixed,
            quantum: CapacityResources {
                cpu_milli: 1,
                mem_bytes: 1,
                disk_bytes: 1,
            },
            overhead: CapacityResources::default(),
            guest_overhead: CapacityResources::default(),
        };
        PoolRecord {
            version: RECORD_VERSION,
            key: BackingKey {
                pool: [1; 32],
                generation: [2; 16],
            },
            execution: Execution::Firecracker,
            cost,
            allocation: Some(cost.allocate(CapacityResources::default()).unwrap()),
            devices: Vec::new(),
            phase: BackingPhase::Granted,
            init: None,
            exit: None,
        }
    }

    fn bounds() -> hex_api::Bounds {
        hex_api::Bounds {
            cpu_max_milli: Some(1000),
            memory_max: Some(hex_api::MIN_MEMORY),
            ..hex_api::Bounds::default()
        }
    }

    fn metadata(root: &Path, record: &PoolRecord) -> PathBuf {
        let name = managed_native_scope_id(NativeRecord::Pool(record)).unwrap();
        let dir = root.join(name.as_str());
        std::fs::create_dir(&dir).unwrap();
        for (name, value) in [
            ("cpu.max", "100000 100000\n".to_owned()),
            ("memory.max", format!("{}\n", hex_api::MIN_MEMORY)),
            ("memory.swap.max", "0\n".to_owned()),
            ("cgroup.events", "populated 0\n".to_owned()),
            ("cgroup.procs", String::new()),
        ] {
            std::fs::write(dir.join(name), value).unwrap();
        }
        dir
    }

    /// AC-T7.1.2: the whole owner metadata set is checked before any recovery callback is allowed.
    #[test]
    fn later_unmapped_native_scope_stops_the_readonly_whole_owner_barrier() {
        let temp = tempfile::tempdir().unwrap();
        let record = pool();
        let retained = [(NativeRecord::Pool(&record), Some(bounds()))];
        let entries = expected(&retained).unwrap();
        let dir = metadata(temp.path(), &record);
        let before = std::fs::read(dir.join("cpu.max")).unwrap();
        preflight(temp.path(), &entries).unwrap();
        std::fs::create_dir(temp.path().join("ffffffffffffffffffffffffffffffff")).unwrap();
        let mut callbacks = 0;
        let result = preflight(temp.path(), &entries).map(|()| callbacks += 1);
        assert!(result.is_err());
        assert_eq!(callbacks, 0);
        assert_eq!(std::fs::read(dir.join("cpu.max")).unwrap(), before);
        assert!(dir.exists());
    }

    /// AC-T7.1.2: actual native limits, complete membership and helper placement must agree.
    #[test]
    fn retained_native_limits_members_and_leaf_scope_are_required_before_adoption() {
        let temp = tempfile::tempdir().unwrap();
        let record = pool();
        let retained = [(NativeRecord::Pool(&record), Some(bounds()))];
        let entries = expected(&retained).unwrap();
        let dir = metadata(temp.path(), &record);
        preflight(temp.path(), &entries).unwrap();
        std::fs::write(
            dir.join("memory.max"),
            format!("{}\n", hex_api::MIN_MEMORY + 1),
        )
        .unwrap();
        assert!(preflight(temp.path(), &entries).is_err());
        std::fs::write(dir.join("memory.max"), format!("{}\n", hex_api::MIN_MEMORY)).unwrap();
        std::fs::write(dir.join("cgroup.events"), "populated 1\n").unwrap();
        std::fs::write(dir.join("cgroup.procs"), "23\n").unwrap();
        assert!(preflight(temp.path(), &entries).is_err()); // No canonical init; no real PID queried.
        std::fs::write(dir.join("cgroup.events"), "populated 0\n").unwrap();
        std::fs::write(dir.join("cgroup.procs"), "").unwrap();
        std::fs::create_dir(dir.join("escaped-child-scope")).unwrap();
        assert!(preflight(temp.path(), &entries).is_err());
        std::fs::remove_dir(dir.join("escaped-child-scope")).unwrap();
        std::fs::remove_file(dir.join("memory.swap.max")).unwrap();
        assert!(preflight(temp.path(), &entries).is_err());
        assert!(dir.exists());
    }

    /// AC-T7.1.2: a controller's exit is not a scope-empty fact; birth and policy remain exact.
    #[test]
    fn controller_exit_and_reused_birth_do_not_authorize_live_helper_cleanup() {
        let mut record = pool();
        record.init = Some(ProcId {
            pid: 7,
            start_ticks: 11,
        });
        let good = |pid| {
            Ok(Some((
                Stat {
                    zombie: false,
                    ppid: 1,
                    start_ticks: if pid == 7 { 11 } else { 12 },
                },
                0,
            )))
        };
        check_members(NativeRecord::Pool(&record), true, &[7, 8], good).unwrap();
        assert!(
            check_members(NativeRecord::Pool(&record), true, &[8], |pid| {
                if pid == 7 { Ok(None) } else { good(pid) }
            })
            .is_err()
        );
        assert!(
            check_members(NativeRecord::Pool(&record), true, &[7, 8], |_| {
                Ok(Some((
                    Stat {
                        zombie: false,
                        ppid: 1,
                        start_ticks: 11,
                    },
                    1,
                )))
            })
            .is_err()
        );
        assert!(check_members(NativeRecord::Pool(&record), false, &[], good).is_err());
        check_members(NativeRecord::Pool(&record), false, &[], |_| Ok(None)).unwrap();
        check_members(NativeRecord::Pool(&record), false, &[], |_| {
            Ok(Some((
                Stat {
                    zombie: true,
                    ppid: 1,
                    start_ticks: 11,
                },
                0,
            )))
        })
        .unwrap();
        assert!(
            check_members(NativeRecord::Pool(&record), true, &[7], |_| Ok(Some((
                Stat {
                    zombie: false,
                    ppid: 1,
                    start_ticks: 99
                },
                0
            ))))
            .is_err()
        );
        record.phase = BackingPhase::Released;
        record.exit = Some(Exit::Code(0));
        assert!(check_members(NativeRecord::Pool(&record), true, &[7], good).is_err());
    }

    /// AC-T7.1.2: a prepared or duplicate incarnation cannot lend native authority to a name.
    #[test]
    fn prepared_and_duplicate_incarnations_cannot_absorb_native_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let mut prepared = pool();
        prepared.phase = BackingPhase::Prepared;
        prepared.allocation = None;
        let retained = [(NativeRecord::Pool(&prepared), None)];
        let entries = expected(&retained).unwrap();
        preflight(temp.path(), &entries).unwrap();
        metadata(temp.path(), &prepared);
        assert!(preflight(temp.path(), &entries).is_err());
        let mut same_scope = pool();
        same_scope.key.pool = [4; 32];
        let retained = [
            (NativeRecord::Pool(&prepared), None),
            (NativeRecord::Pool(&same_scope), Some(bounds())),
        ];
        assert!(expected(&retained).is_err());
    }

    /// AC-T7.4.1: cancellation requires both a canonical ungranted lifetime and this owner's
    /// exact scope absence. A sibling is not a singleton conflict; any named object/presence or
    /// observation error for the target refuses, without removing or adopting anything.
    #[test]
    fn unsubmitted_prepared_pool_requires_no_slot_and_exact_native_metadata_absence() {
        let temp = tempfile::tempdir().unwrap();
        let mut record = pool();
        record.phase = BackingPhase::Prepared;
        record.allocation = None;
        let native = NativeRecord::Pool(&record);
        let name = unsubmitted_pool_name(native, false).unwrap();
        std::fs::create_dir(temp.path().join("unrelated-sibling")).unwrap();
        check_unsubmitted_pool_path(temp.path(), name.as_str(), native).unwrap();
        assert!(unsubmitted_pool_name(native, true).is_err());
        let path = temp.path().join(name.as_str());
        std::fs::create_dir(&path).unwrap();
        assert!(check_unsubmitted_pool_path(temp.path(), name.as_str(), native).is_err());
        assert!(path.is_dir());
        std::fs::remove_dir(&path).unwrap();
        std::fs::write(&path, b"retained-positive-metadata").unwrap();
        assert!(check_unsubmitted_pool_path(temp.path(), name.as_str(), native).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"retained-positive-metadata");
        std::fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(temp.path().join("missing-target"), &path).unwrap();
        assert!(check_unsubmitted_pool_path(temp.path(), name.as_str(), native).is_err());
        std::fs::remove_file(&path).unwrap();
        let missing_root = temp.path().join("missing-root");
        assert!(check_unsubmitted_pool_path(&missing_root, name.as_str(), native).is_err());
        let linked_root = temp.path().join("linked-root");
        std::os::unix::fs::symlink(temp.path(), &linked_root).unwrap();
        assert!(check_unsubmitted_pool_path(&linked_root, name.as_str(), native).is_err());
        for phase in [
            BackingPhase::Granted,
            BackingPhase::Live,
            BackingPhase::Draining,
            BackingPhase::Released,
        ] {
            let mut changed = pool();
            changed.phase = phase;
            if phase != BackingPhase::Granted {
                changed.init = Some(ProcId {
                    pid: 7,
                    start_ticks: 11,
                });
            }
            if phase == BackingPhase::Released {
                changed.exit = Some(Exit::Code(0));
            }
            changed.check().unwrap();
            assert!(unsubmitted_pool_name(NativeRecord::Pool(&changed), false).is_err());
        }
        record.allocation = Some(record.cost.allocate(CapacityResources::default()).unwrap());
        assert!(unsubmitted_pool_name(NativeRecord::Pool(&record), false).is_err());
        assert!(temp.path().join("unrelated-sibling").is_dir());
    }

    /// AC-T7.4.1: cancellation and later Index ACK are two observations of the SAME unallocated
    /// lifetime. New metadata between them retains the row; no fake exit or native release proof.
    #[test]
    fn canceled_unallocated_pool_ack_rechecks_exact_absence_and_retains_new_metadata() {
        let temp = tempfile::tempdir().unwrap();
        let mut record = pool();
        record.phase = BackingPhase::Prepared;
        record.allocation = None;
        let name = unsubmitted_pool_name(NativeRecord::Pool(&record), false).unwrap();
        check_unsubmitted_pool_path(temp.path(), name.as_str(), NativeRecord::Pool(&record))
            .unwrap();
        record.phase = BackingPhase::Released;
        record.check().unwrap();
        let native = NativeRecord::Pool(&record);
        assert_eq!(unsubmitted_pool_name(native, false).unwrap(), name);
        assert!(record.init.is_none() && record.exit.is_none() && record.allocation.is_none());
        assert!(unsubmitted_pool_name(native, true).is_err());
        let path = temp.path().join(name.as_str());
        std::fs::create_dir(&path).unwrap();
        assert!(check_unsubmitted_pool_path(temp.path(), name.as_str(), native).is_err());
        assert!(path.is_dir());
        std::fs::remove_dir(&path).unwrap();
        check_unsubmitted_pool_path(temp.path(), name.as_str(), native).unwrap();
        assert!(!path.exists());
    }

    /// AC-T7.1.2: real proc stat command bytes never replace the exact PID/birth/policy suffix.
    #[test]
    fn native_stat_command_bytes_do_not_replace_the_exact_birth_and_policy() {
        let mut fields = vec!["0"; 50];
        fields[0] = "S";
        fields[1] = "1";
        fields[19] = "123";
        fields[38] = "3";
        let mut bytes = b"7 (vmm )\xff ".to_vec();
        bytes.extend_from_slice(b") ");
        bytes.extend_from_slice(fields.join(" ").as_bytes());
        bytes.push(b'\n');
        let (stat, policy) = parse_stat(&bytes, 7).unwrap();
        assert_eq!(stat.start_ticks, 123);
        assert_eq!(policy, 3);
        assert!(parse_stat(&bytes, 8).is_err());
        bytes.pop();
        assert!(parse_stat(&bytes, 7).is_err());
    }
}
