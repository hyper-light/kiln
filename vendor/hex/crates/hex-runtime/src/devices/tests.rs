use super::*;
use hex_api::{Bounds, ClassSpec};
use hex_core::record::Phase;

fn catalog() -> Catalog {
    Catalog::parse(b"hex-devices-1\ngpu\t256\tfixture-a\t4096\tcdi\t/private/fixture.json\tfixture.test/gpu=a\ngpu\t512\tfixture-b\t8192\thcs\tPCIROOT(0)#PCI(0200)\t\n").unwrap()
}

fn spec(name: &str, key: u64) -> BoxSpec {
    BoxSpec {
        id: BoxId::new(name).unwrap(),
        bounds: Bounds::default(),
        class: ClassSpec::Oci(Box::new(hex_api::OciSpec {
            rootfs: hex_api::Rootfs::Dir("/fixture/root".into()),
            init: hex_api::Program::of(&["true"]),
            mounts: Vec::new(),
            hostname: None,
            network: hex_api::Network::Private,
            hosts: Vec::new(),
        })),
        owner: None,
        allocation: None,
        route: None,
        devices: vec![DeviceId {
            kind: DeviceKind::Gpu,
            key,
        }],
    }
}

fn pool(phase: hex_api::BackingPhase) -> PoolRecord {
    let guest = hex_api::CapacityResources {
        cpu_milli: 1000,
        mem_bytes: hex_api::MIN_MEMORY,
        disk_bytes: 0,
    };
    let cost = hex_api::RuntimeCost {
        id: [7; 32],
        floor: guest,
        ceiling: guest,
        quantum: hex_api::CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1,
            disk_bytes: 1,
        },
        guest_overhead: hex_api::CapacityResources::default(),
        overhead: hex_api::CapacityResources::default(),
    };
    PoolRecord {
        version: hex_core::record::RECORD_VERSION,
        key: hex_api::BackingKey {
            pool: [8; 32],
            generation: [9; 16],
        },
        execution: hex_api::Execution::ShardsDedicated,
        cost,
        allocation: (phase != hex_api::BackingPhase::Prepared).then(|| {
            cost.allocate(hex_api::CapacityResources::default())
                .unwrap()
        }),
        devices: vec![DeviceId {
            kind: DeviceKind::Gpu,
            key: 256,
        }],
        phase,
        init: if matches!(
            phase,
            hex_api::BackingPhase::Live
                | hex_api::BackingPhase::Draining
                | hex_api::BackingPhase::Released
        ) {
            Some(hex_core::record::ProcId {
                pid: 1,
                start_ticks: 1,
            })
        } else {
            None
        },
        exit: if phase == hex_api::BackingPhase::Released {
            Some(hex_api::Exit::Unknown)
        } else {
            None
        },
    }
}

/// AC-T7.4.1: synthetic native receipts exercise the SAME catalog owner join, not native proof.
#[test]
fn canonical_pool_holder_retains_grant_until_released_and_fences_exact_handoff() {
    use hex_api::{
        BackingPhase, DeviceHolder, Execution, GpuEligibility, ImageArch, ImageOs, ImagePlatform,
        RouteKey, RouteReceipt,
    };
    let catalog = catalog();
    let id = DeviceId {
        kind: DeviceKind::Gpu,
        key: 256,
    };
    for phase in [
        BackingPhase::Prepared,
        BackingPhase::Granted,
        BackingPhase::Live,
        BackingPhase::Draining,
        BackingPhase::Released,
    ] {
        let pool = pool(phase);
        pool.check().unwrap();
        let key = pool.key;
        let records = vec![pool];
        let holder = catalog.holder(id, &Records::new(), &records).unwrap();
        assert_eq!(
            holder,
            if !matches!(phase, BackingPhase::Prepared | BackingPhase::Released) {
                Some(DeviceHolder::Backing(key))
            } else {
                None
            }
        );
    }
    let live = pool(BackingPhase::Live);
    let mut selected = spec("handoff", 256);
    selected.bounds.cpu_max_milli = Some(hex_api::MIN_CPU_MILLI);
    selected.bounds.memory_max = Some(hex_api::MIN_MEMORY);
    selected.allocation = live.allocation;
    selected.route = Some(RouteReceipt {
        key: RouteKey {
            class: hex_api::Class::Oci,
            image: ImagePlatform {
                os: ImageOs::Linux,
                arch: ImageArch::Amd64,
            },
            execution: Execution::ShardsDedicated,
            gpu: GpuEligibility::Required,
        },
        backing: Some(live.key),
    });
    let pools = vec![live];
    assert!(catalog.prepare(&selected, &Records::new(), &pools).is_ok());
    let mut record = BoxRecord::new(selected.clone(), Phase::Creating, Vec::new());
    record.native_scope = Some(hex_core::record::NativeScope::from_bytes([10; 16]));
    let boxes = Records::from([(selected.id.clone(), Ok(record))]);
    assert_eq!(
        catalog.holder(id, &boxes, &pools).unwrap(),
        Some(DeviceHolder::Backing(pools[0].key))
    );
    selected
        .route
        .as_mut()
        .unwrap()
        .backing
        .as_mut()
        .unwrap()
        .generation = [11; 16];
    assert!(catalog.prepare(&selected, &Records::new(), &pools).is_err());
    let boxes = Records::from([(
        selected.id.clone(),
        Ok(BoxRecord::new(selected, Phase::Creating, Vec::new())),
    )]);
    assert!(
        catalog.holder(id, &boxes, &pools).is_err(),
        "stale generation cannot turn a conflicting owner into free capacity"
    );
    let duplicate = vec![pool(BackingPhase::Live), pool(BackingPhase::Live)];
    assert!(catalog.holder(id, &Records::new(), &duplicate).is_err());
}

/// AC-T7.4.1: a request selects the configured physical resource, never its neighbor's edit plan.
#[test]
fn selected_identity_returns_only_its_native_binding() {
    let catalog = catalog();
    let selected = catalog
        .prepare(&spec("one", 512), &Records::new(), &[])
        .unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!(selected[0].info.id.key, 512);
    assert!(
        matches!(selected[0].binding(Route::Hcs), Some(Binding::Hcs { location }) if location == "PCIROOT(0)#PCI(0200)")
    );
    let selected = catalog
        .prepare(&spec("two", 256), &Records::new(), &[])
        .unwrap();
    assert!(
        matches!(selected[0].binding(Route::Cdi), Some(Binding::Cdi { file, qualified }) if file == "/private/fixture.json" && qualified == "fixture.test/gpu=a")
    );
    assert!(
        catalog
            .prepare(&spec("unknown", 768), &Records::new(), &[])
            .is_err()
    );
}

/// AC-T7.4.1/T-T7.1.2: pending creation and deletion retain exactly the same exclusive resource.
#[test]
fn every_recorded_phase_retains_the_device_until_physical_removal() {
    let catalog = catalog();
    let held = spec("held", 256);
    for phase in [Phase::Creating, Phase::Ready, Phase::Deleting] {
        let record = BoxRecord::new(held.clone(), phase, Vec::new());
        let bytes = record.encode().unwrap();
        let records = BTreeMap::from([(held.id.clone(), Ok(record))]);
        assert_eq!(
            catalog.holder(held.devices[0], &records, &[]).unwrap(),
            Some(hex_api::DeviceHolder::Box(held.id.try_clone().unwrap()))
        );
        assert!(catalog.prepare(&spec("next", 256), &records, &[]).is_err());
        assert!(
            catalog
                .prepare(&spec("neighbor", 512), &records, &[])
                .is_ok()
        );
        assert_eq!(
            records
                .get(&held.id)
                .unwrap()
                .as_ref()
                .unwrap()
                .encode()
                .unwrap(),
            bytes
        );
    }
    assert!(
        catalog
            .prepare(&spec("after-removal", 256), &Records::new(), &[])
            .is_ok()
    );
}

/// AC-T7.4.1: recovery uncertainty never becomes invented free capacity, and reads change no rows.
#[test]
fn malformed_records_and_multiple_holders_refuse_without_repairing_them() {
    let catalog = catalog();
    let first = spec("one", 256);
    let second = spec("two", 256);
    let records = BTreeMap::from([
        (
            first.id.clone(),
            Ok(BoxRecord::new(first, Phase::Ready, Vec::new())),
        ),
        (
            second.id.clone(),
            Ok(BoxRecord::new(second, Phase::Deleting, Vec::new())),
        ),
    ]);
    assert!(matches!(
        catalog.prepare(&spec("next", 256), &records, &[]),
        Err(Refusal::Broken(_))
    ));
    assert_eq!(records.len(), 2);
    let broken = BTreeMap::from([(
        BoxId::new("damaged").unwrap(),
        Err("private corrupt content".into()),
    )]);
    let error = catalog
        .prepare(&spec("next", 512), &broken, &[])
        .err()
        .unwrap();
    assert!(matches!(error, Refusal::Broken(_)));
    assert!(!error.to_string().contains("private corrupt content"));
    assert_eq!(broken.len(), 1);
}

/// AC-T7.4.1: malformed administrator data returns only a field/cause, never private bindings.
#[test]
fn invalid_catalogs_refuse_without_echoing_private_configuration() {
    for bytes in [
        b"".as_slice(),
        b"hex-devices-2\n",
        b"hex-devices-1\ngpu\tbad\tprivate-model\t4096\tcdi\t/private/secret\tsecret-name\n",
        b"hex-devices-1\ngpu\t256\tprivate-model\tbad\tcdi\t/private/secret\tsecret-name\n",
        b"hex-devices-1\nnet\t0\tprivate-model\t0\tcdi\t/private/secret\tsecret-name\n",
        b"hex-devices-1\ngpu\t256\tprivate-model\t0\thcs\tprivate-location\tnot-empty\n",
        b"hex-devices-1\ngpu\t256\tprivate-model\t0\tcdi\trelative-secret\tsecret-name\n",
        b"hex-devices-1\ngpu\t256\tprivate-model\t0\tcdi\t/private/secret\tsecret-name\textra\n",
    ] {
        let error = Catalog::parse(bytes).err().unwrap();
        let rendered = format!("{error:?} {error}");
        for private in [
            "private-model",
            "private-location",
            "/private/secret",
            "secret-name",
            "relative-secret",
        ] {
            assert!(!rendered.contains(private), "{rendered}");
        }
    }
    let duplicate = b"hex-devices-1\ngpu\t256\ta\t1\tcdi\t/fixture\tfixture.test/gpu=a\ngpu\t256\tb\t2\tcdi\t/other\tfixture.test/gpu=b\n";
    assert!(Catalog::parse(duplicate).is_err());
}

/// AC-T7.4.1: absence gives no grants; an oversized/growing input cannot be silently truncated.
#[test]
fn absent_and_oversized_catalogs_are_distinguished() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("devices");
    assert!(Catalog::open(&path).unwrap().entries().is_empty());
    std::fs::write(&path, []).unwrap();
    assert!(Catalog::open(&path).is_err());
    let bytes = vec![b'x'; hex_core::bundle::MAX_CONFIG_BYTES + 1];
    std::fs::write(&path, bytes).unwrap();
    assert!(Catalog::open(&path).is_err());
    std::fs::write(&path, b"hex-devices-1\n").unwrap();
    assert!(Catalog::open(&path).unwrap().entries().is_empty());
}

/// AC-T7.4.1: OCI and microVM routes for one physical GPU share one slot and retained holder.
#[test]
fn explicit_consistent_backend_aliases_keep_one_physical_entry_and_holder() {
    let catalog = Catalog::parse(b"hex-devices-1\ngpu\t512\tother\t8192\thcs\tPCIROOT(0)#PCI(0200)\t\ngpu\t256\tsame\t4096\tshards-gpu\tpci:0000:01:00.0\t\ngpu\t256\tsame\t4096\tcdi\t/private/gpu.cdi\tfixture.test/gpu=a\ngpu\t256\tsame\t4096\tvfio\t0000:01:00.0\t7\n").unwrap();
    assert_eq!(catalog.entries().len(), 2);
    assert_eq!(catalog.entries()[0].info.id.key, 512);
    assert_eq!(catalog.entries()[1].info.id.key, 256);
    for route in [Route::Cdi, Route::ShardsGpu, Route::Vfio] {
        let entries = catalog.entries_for(route).collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].info.id.key, 256);
        assert_eq!(entries[0].bindings().len(), 3);
    }
    assert_eq!(catalog.entries_for(Route::Hcs).count(), 1);
    let held = spec("retained-alias", 256);
    let records = BTreeMap::from([(
        held.id.clone(),
        Ok(BoxRecord::new(held.clone(), Phase::Creating, Vec::new())),
    )]);
    assert_eq!(
        catalog.holder(held.devices[0], &records, &[]).unwrap(),
        Some(hex_api::DeviceHolder::Box(held.id.try_clone().unwrap()))
    );
    assert!(
        catalog
            .prepare(&spec("another-backend", 256), &records, &[])
            .is_err()
    );
    assert!(
        catalog
            .prepare(&spec("independent", 512), &records, &[])
            .is_ok()
    );
    assert_eq!(catalog.conflict_ids(&held.devices).unwrap(), held.devices);
}

/// AC-T7.4.1: aliases cannot change physical metadata or make one backend choose between recipes.
#[test]
fn conflicting_alias_metadata_and_duplicate_routes_refuse() {
    let first = "hex-devices-1\ngpu\t256\tsame\t4096\tcdi\t/private/a\tfixture.test/gpu=a\n";
    for alias in [
        "gpu\t256\tchanged\t4096\tshards-gpu\tpci:0000:01:00.0\t\n",
        "gpu\t256\tsame\t8192\tshards-gpu\tpci:0000:01:00.0\t\n",
        "gpu\t256\tsame\t4096\tcdi\t/private/a\tfixture.test/gpu=a\n",
        "gpu\t256\tsame\t4096\tcdi\t/private/other\tfixture.test/gpu=b\n",
    ] {
        assert!(Catalog::parse(format!("{first}{alias}").as_bytes()).is_err());
    }
}

/// AC-T7.4.1: a current-boot LUID declaration does not rename the stable identity or prove a join.
#[test]
fn luid_route_retains_stable_device_identity_without_claiming_native_correspondence() {
    let catalog = Catalog::parse(
        b"hex-devices-1\ngpu\t256\tsame\t4096\tshards-gpu\tluid:123456789abcdef0\t\n",
    )
    .unwrap();
    let entry = catalog.entries().first().unwrap();
    assert_eq!(entry.info.id.key, 256);
    assert!(matches!(
        entry.binding(Route::ShardsGpu),
        Some(Binding::ShardsGpu {
            physical: shards_api::PhysicalGpu::Luid(0x123456789abcdef0)
        })
    ));
}

/// AC-T7.4.1: a catalog label cannot grant trusted guest render authority or bypass host CDI.
#[test]
fn guest_recipe_is_separate_and_requires_exact_boot_owner_and_full_set() {
    let catalog = Catalog::parse(b"hex-devices-1\ngpu\t256\tshards-renderer\t0\tguest-gpu\t/private/guest.cdi\thex.shards/gpu=256\n").unwrap();
    assert_eq!(catalog.entries_for(Route::GuestGpu).count(), 1);
    assert_eq!(catalog.entries_for(Route::Cdi).count(), 0);
    let selected = spec("dedicated", 256);
    assert!(catalog.cdi(&selected.devices).is_err());
    let foreign = crate::guest::gpu::Authority {
        owner: BoxId::new("foreign").unwrap(),
        devices: selected.devices.clone(),
    };
    assert!(catalog.guest_cdi(&selected, &foreign).is_err());
    let subset = crate::guest::gpu::Authority {
        owner: selected.id.clone(),
        devices: vec![
            selected.devices[0],
            DeviceId {
                kind: DeviceKind::Gpu,
                key: 512,
            },
        ],
    };
    assert!(catalog.guest_cdi(&selected, &subset).is_err());
    for row in [
        "gpu\t256\tshards-renderer\t0\tguest-gpu\t/private/g\thex.shards/gpu=512",
        "gpu\t256\tshards-renderer\t4096\tguest-gpu\t/private/g\thex.shards/gpu=256",
        "gpu\t256\tinvented-measured-model\t0\tguest-gpu\t/private/g\thex.shards/gpu=256",
    ] {
        assert!(Catalog::parse(format!("hex-devices-1\n{row}\n").as_bytes()).is_err());
    }
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("guest.cdi");
    std::fs::write(&path, br#"{"cdiVersion":"1.1.0","kind":"hex.shards/gpu","devices":[{"name":"256","containerEdits":{"deviceNodes":[{"path":"/dev/dri/renderD128","type":"c","major":226,"minor":128,"permissions":"rw"}]}}]}"#).unwrap();
    let catalog = Catalog::parse(
        format!(
            "hex-devices-1\ngpu\t256\tshards-renderer\t0\tguest-gpu\t{}\thex.shards/gpu=256\n",
            path.display()
        )
        .as_bytes(),
    )
    .unwrap();
    let authority = crate::guest::gpu::Authority {
        owner: selected.id.clone(),
        devices: selected.devices.clone(),
    };
    let plan = catalog.guest_cdi(&selected, &authority).unwrap();
    assert_eq!(plan.edits().unwrap().device_nodes.len(), 1);
    assert!(catalog.cdi(&selected.devices).is_err());
}

/// AC-T7.4.1: a strict subset is one consuming reference while the original full source owns P.
#[test]
fn subset_handoff_keeps_full_pool_holder_and_one_consumer_until_scope_released() {
    use hex_api::{
        BackingPhase, DeviceHolder, Execution, GpuEligibility, ImageArch, ImageOs, ImagePlatform,
        LifecycleCause, RouteKey, RouteReceipt,
    };
    let catalog = catalog();
    let mut source = pool(BackingPhase::Live);
    source.devices.push(DeviceId {
        kind: DeviceKind::Gpu,
        key: 512,
    });
    let key = source.key;
    let p = source.devices.clone();
    let mut selected = spec("subset", 256);
    selected.bounds.cpu_max_milli = Some(hex_api::MIN_CPU_MILLI);
    selected.bounds.memory_max = Some(hex_api::MIN_MEMORY);
    selected.allocation = source.allocation;
    selected.route = Some(RouteReceipt {
        key: RouteKey {
            class: hex_api::Class::Oci,
            image: ImagePlatform {
                os: ImageOs::Linux,
                arch: ImageArch::Amd64,
            },
            execution: Execution::ShardsDedicated,
            gpu: GpuEligibility::Required,
        },
        backing: Some(key),
    });
    assert_eq!(
        catalog
            .prepare(&selected, &Records::new(), std::slice::from_ref(&source))
            .unwrap()
            .len(),
        1
    );
    let mut pending = BoxRecord::new(selected.clone(), Phase::Creating, Vec::new());
    pending.native_scope = Some(hex_core::record::NativeScope::from_bytes([42; 16]));
    let mut records = Records::from([(selected.id.clone(), Ok(pending))]);
    for id in &p {
        assert_eq!(
            catalog
                .holder(*id, &records, std::slice::from_ref(&source))
                .unwrap(),
            Some(DeviceHolder::Backing(key))
        );
    }
    let mut sibling = selected.clone();
    sibling.id = BoxId::new("disjoint-consumer").unwrap();
    sibling.devices = vec![p[1]];
    assert!(
        matches!(catalog.prepare(&sibling, &records, std::slice::from_ref(&source)), Err(Refusal::Backing {key:got,cause:LifecycleCause::Busy}) if got==key)
    );
    records
        .get_mut(&selected.id)
        .unwrap()
        .as_mut()
        .unwrap()
        .phase = Phase::Deleting;
    source.phase = BackingPhase::Draining;
    for id in &p {
        assert_eq!(
            catalog
                .holder(*id, &records, std::slice::from_ref(&source))
                .unwrap(),
            Some(DeviceHolder::Backing(key))
        );
    }
    source.phase = BackingPhase::Released;
    source.exit = Some(hex_api::Exit::Code(0));
    assert_eq!(
        catalog
            .holder(p[0], &records, std::slice::from_ref(&source))
            .unwrap(),
        Some(DeviceHolder::Box(selected.id.clone()))
    );
    assert_eq!(
        catalog
            .holder(p[1], &records, std::slice::from_ref(&source))
            .unwrap(),
        None
    );
    assert!(
        catalog
            .prepare(&selected, &records, std::slice::from_ref(&source))
            .is_ok()
    );
    let mut changed = selected.clone();
    if let ClassSpec::Oci(oci) = &mut changed.class {
        oci.hostname = Some("changed".into());
    }
    assert!(
        catalog
            .prepare(&changed, &records, std::slice::from_ref(&source))
            .is_err()
    );
    source.phase = BackingPhase::Live;
    source.exit = None;
    records
        .get_mut(&selected.id)
        .unwrap()
        .as_mut()
        .unwrap()
        .phase = Phase::Ready;
    assert!(
        catalog
            .holder(p[0], &records, std::slice::from_ref(&source))
            .is_err()
    );
}

/// AC-T7.4.1: provisioning uses one canonical unheld set, independent of telemetry/workload selection.
#[test]
fn deterministic_family_complement_respects_current_claims_references_and_group_aliases() {
    use hex_api::{BackingPhase, LifecycleCause};
    let catalog = Catalog::parse(b"hex-devices-1\ngpu\t256\ta\t4096\tvfio\t0000:01:00.0\t7\ngpu\t257\tb\t4096\tvfio\t0000:01:00.1\t7\ngpu\t512\tc\t8192\thcs\tPCIROOT(0)#PCI(0200)\t\n").unwrap();
    let id = |key| DeviceId {
        kind: DeviceKind::Gpu,
        key,
    };
    let full = [id(256), id(512)];
    let current = pool(BackingPhase::Live);
    let key = current.key;
    let mut pools = vec![current];
    let mut records = Records::new();
    let box_s = spec("selected-s", 512);
    records.insert(
        box_s.id.clone(),
        Ok(BoxRecord::new(box_s, Phase::Ready, Vec::new())),
    );
    assert_eq!(
        catalog
            .unheld_subset(&full, Some(key), &records, &pools)
            .unwrap(),
        vec![id(256)]
    );
    records.clear();
    assert_eq!(
        catalog
            .unheld_subset(&full, Some(key), &records, &pools)
            .unwrap(),
        full
    );
    assert_eq!(
        catalog
            .unheld_subset(&full, None, &records, &pools)
            .unwrap(),
        vec![id(512)]
    );

    // Prepared owns no hardware. A Box claim on a catalogued companion excludes its
    // inseparable family member from this proposal's desired complement.
    pools[0] = pool(BackingPhase::Prepared);
    let boxed = spec("foreign-alias", 257);
    records.insert(
        boxed.id.clone(),
        Ok(BoxRecord::new(boxed, Phase::Deleting, Vec::new())),
    );
    assert_eq!(
        catalog
            .unheld_subset(&full, Some(key), &records, &pools)
            .unwrap(),
        vec![id(512)]
    );
    records.clear();
    pools[0] = pool(BackingPhase::Live);
    let mut selected = spec("handoff-reference", 256);
    selected.bounds.cpu_max_milli = Some(hex_api::MIN_CPU_MILLI);
    selected.bounds.memory_max = Some(hex_api::MIN_MEMORY);
    selected.allocation = pools[0].allocation;
    selected.route = Some(hex_api::RouteReceipt {
        key: hex_api::RouteKey {
            class: hex_api::Class::Oci,
            image: hex_api::ImagePlatform {
                os: hex_api::ImageOs::Linux,
                arch: hex_api::ImageArch::Amd64,
            },
            execution: hex_api::Execution::ShardsDedicated,
            gpu: hex_api::GpuEligibility::Required,
        },
        backing: Some(key),
    });
    let mut pending = BoxRecord::new(selected, Phase::Creating, Vec::new());
    pending.native_scope = Some(hex_core::record::NativeScope::from_bytes([11; 16]));
    records.insert(pending.spec.id.clone(), Ok(pending));
    assert!(
        matches!(catalog.unheld_subset(&full, Some(key), &records, &pools), Err(Refusal::Backing { key: actual, cause: LifecycleCause::Busy }) if actual == key)
    );
    records.clear();
    pools[0] = pool(BackingPhase::Released);
    let boxed = spec("released-parent-s", 512);
    records.insert(
        boxed.id.clone(),
        Ok(BoxRecord::new(boxed, Phase::Ready, Vec::new())),
    );
    assert_eq!(
        catalog
            .unheld_subset(&full, None, &records, &pools)
            .unwrap(),
        vec![id(256)]
    );
    assert!(
        catalog
            .unheld_subset(&full, Some(key), &records, &pools)
            .is_err()
    );
    let mut foreign = pool(BackingPhase::Draining);
    foreign.key.pool = [10; 32];
    pools.push(foreign);
    assert!(
        catalog
            .unheld_subset(&full, None, &records, &pools)
            .unwrap()
            .is_empty()
    );
    records.clear();
    records.insert(
        BoxId::new("malformed").unwrap(),
        Err("retained unread row".into()),
    );
    assert!(matches!(
        catalog.unheld_subset(&full, None, &records, &pools),
        Err(Refusal::Lifecycle {
            cause: LifecycleCause::Uncertain,
            ..
        })
    ));
    assert!(
        catalog
            .unheld_subset(&[id(512), id(256)], None, &Records::new(), &[])
            .is_err()
    );
    assert!(
        catalog
            .unheld_subset(&[id(256), id(256)], None, &Records::new(), &[])
            .is_err()
    );
    assert!(
        catalog
            .unheld_subset(&[], None, &Records::new(), &[])
            .unwrap()
            .is_empty()
    );
}

/// AC-T7.4.1: exact tuple union includes real native/FC/Shards forms and referenced history.
#[test]
fn route_projection_unions_mixed_producers_and_distinct_lifetimes() {
    use hex_api::{
        BackingKey, Class, Execution, GpuEligibility, ImageArch, ImageOs, ImagePlatform, RouteKey,
    };
    let image = ImagePlatform {
        os: ImageOs::Linux,
        arch: ImageArch::Amd64,
    };
    let mut projection = RouteProjection::new();
    let (process, oci) = native_route_keys(Class::Oci, image).unwrap();
    projection.retain(process, None).unwrap();
    projection.retain(oci.unwrap(), None).unwrap();
    projection.retain(process, None).unwrap();
    assert_eq!(projection.count().unwrap(), 2);
    let shards = RouteKey {
        class: Class::Shards,
        image,
        execution: Execution::ShardsDedicated,
        gpu: GpuEligibility::Required,
    };
    let oci_guest = RouteKey {
        class: Class::Oci,
        ..shards
    };
    let fc = RouteKey {
        class: Class::Firecracker,
        execution: Execution::Firecracker,
        ..shards
    };
    let parent = BackingKey {
        pool: [1; 32],
        generation: [2; 16],
    };
    let current = BackingKey {
        pool: [3; 32],
        generation: [4; 16],
    };
    for key in [shards, oci_guest] {
        projection.retain(key, Some(parent)).unwrap();
        projection.retain(key, Some(current)).unwrap();
    }
    projection
        .retain(
            fc,
            Some(BackingKey {
                pool: [5; 32],
                generation: [6; 16],
            }),
        )
        .unwrap();
    assert_eq!(projection.count().unwrap(), 7);
    projection.introduce(shards).unwrap();
    projection.introduce(oci_guest).unwrap();
    projection.introduce(shards).unwrap();
    assert_eq!(projection.count().unwrap(), 9);
    assert!(projection.retain(process, Some(parent)).is_err());
    assert!(projection.introduce(oci.unwrap()).is_err());
    // This models actual absence: no caller invokes a Mac native producer that has no capacity.
    let absent = RouteProjection::new();
    assert_eq!(absent.count().unwrap(), 0);
}

/// AC-T7.4.1: protocol bound applies to complete tuple union BEFORE another slot allocation.
#[test]
fn route_projection_accepts_exact_word_and_refuses_overbound_without_fake_generation() {
    use hex_api::{
        BackingKey, Class, Execution, GpuEligibility, ImageArch, ImageOs, ImagePlatform, RouteKey,
    };
    let key = RouteKey {
        class: Class::Firecracker,
        image: ImagePlatform {
            os: ImageOs::Linux,
            arch: ImageArch::Amd64,
        },
        execution: Execution::Firecracker,
        gpu: GpuEligibility::Required,
    };
    let mut projection = RouteProjection::new();
    for i in 0..hex_api::MAX_RUNTIME_ROUTES {
        let pool = u8::try_from(i).unwrap();
        projection
            .retain(
                key,
                Some(BackingKey {
                    pool: [pool; 32],
                    generation: [1; 16],
                }),
            )
            .unwrap();
    }
    assert_eq!(projection.count().unwrap(), hex_api::MAX_RUNTIME_ROUTES);
    projection
        .retain(
            key,
            Some(BackingKey {
                pool: [0; 32],
                generation: [1; 16],
            }),
        )
        .unwrap();
    assert!(matches!(
        projection.introduce(key),
        Err(Refusal::Exhausted {
            bound: hex_api::MAX_RUNTIME_ROUTES,
            ..
        })
    ));
    assert_eq!(projection.count().unwrap(), hex_api::MAX_RUNTIME_ROUTES);
    assert!(
        projection
            .retain(
                key,
                Some(BackingKey {
                    pool: [0; 32],
                    generation: [2; 16]
                })
            )
            .is_err()
    );
    assert_eq!(projection.count().unwrap(), hex_api::MAX_RUNTIME_ROUTES);
}
