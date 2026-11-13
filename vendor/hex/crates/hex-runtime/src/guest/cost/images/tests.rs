use super::parse;
use crate::guest::{GuestConfig, GuestRuntime, shares::Share};
use hex_api::{CapacityResources, RuntimeCost};
use sha2::{Digest as _, Sha256};
use std::sync::Arc;

fn row(role: &str, bytes: &[u8]) -> String {
    let hash: String = Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{role} {} {hash}\n", bytes.len())
}

fn evidence() -> Vec<u8> {
    let mut bytes = b"hex-shards-native-images 1\n".to_vec();
    for (role, image) in [
        ("cli", b"cli".as_slice()),
        ("kernel", b"kernel"),
        ("cpu-vmm", b"vm"),
    ] {
        bytes.extend_from_slice(row(role, image).as_bytes());
    }
    bytes.extend_from_slice(b"gpu-vmm none\n\nopaque model measurement body\x00\xff\n");
    bytes
}

fn cost(id: [u8; 32]) -> RuntimeCost {
    // Pure fixture inputs, not measured overhead or an accepted runtime profile.
    let fixed = CapacityResources {
        cpu_milli: 1000,
        mem_bytes: 256 << 20,
        disk_bytes: 0,
    };
    RuntimeCost {
        id,
        floor: fixed,
        ceiling: fixed,
        quantum: CapacityResources {
            cpu_milli: 1,
            mem_bytes: 1 << 20,
            disk_bytes: 1,
        },
        guest_overhead: CapacityResources::default(),
        overhead: CapacityResources::default(),
    }
}

/// AC-T7.4.1: every original evidence byte contributes to the cost identity;
/// typed input reaches the real initialized runtime and pure PreparedBoot.
#[test]
fn native_evidence_joins_same_cost_and_real_preparation_without_rewriting() {
    let dir = tempfile::tempdir().unwrap();
    let evidence = evidence();
    let original = evidence.clone();
    let cost = cost(Sha256::digest(&evidence).into());
    let file = dir.path().join("cost");
    let proof = dir.path().join("evidence");
    let encoded = serde_json::to_vec(&cost).unwrap();
    std::fs::write(&file, &encoded).unwrap();
    std::fs::write(&proof, &evidence).unwrap();
    let (loaded, images) =
        super::super::load_runtime_inputs(&file, Sha256::digest(&encoded).into(), &proof).unwrap();
    assert_eq!(loaded, cost);
    assert_eq!(std::fs::read(&proof).unwrap(), original);
    let images = images.unwrap();
    assert_eq!(images.evidence(), cost.id);
    assert_eq!(
        images.cli().sha256(),
        <[u8; 32]>::from(Sha256::digest(b"cli"))
    );
    assert!(images.vmm(true).is_err());
    let config = GuestConfig {
        binary: dir.path().join("cli"),
        kernel: dir.path().join("kernel"),
        initrd: dir.path().join("init"),
        vcpus: 1,
        memory_mib: 256,
        gpu: false,
        shares: vec![Share {
            dir: dir.path().join("shared"),
            at: "/shared".into(),
            read_only: true,
        }],
        staged: "/run/hex/staged".into(),
    };
    std::fs::write(&config.kernel, b"kernel").unwrap();
    let paths =
        shards_api::RuntimePaths::new(dir.path().join("cache"), dir.path().join("home")).unwrap();
    let runtime = GuestRuntime::new(Arc::new(shards_api::Shards::new()), config.clone())
        .with_runtime_paths(paths)
        .with_runtime_cost(cost)
        .unwrap()
        .with_native_images(images)
        .unwrap();
    // Pure preparation creates/dials no socket; avoid long private TMPDIR paths.
    let socket = dir
        .path()
        .ancestors()
        .last()
        .unwrap()
        .join("native-model.v");
    let budget = crate::guest::managed::Budget::new(runtime.first_dial_bound().unwrap()).unwrap();
    let plan = runtime
        .managed_plan(&config.spec(), &socket, budget)
        .unwrap();
    let view = plan.native_startup().unwrap();
    assert_eq!(view.images(), images);
    assert_eq!(view.kernel(), config.kernel);
    assert_eq!(view.console_pin(), images.kernel().sha256());
    assert_eq!(view.shares().len(), 1);
    assert_eq!(
        view.shares().first().unwrap().dir,
        config.shares.first().unwrap().dir
    );
    assert!(view.shares().first().unwrap().read_only);
    let changed = RuntimeCost {
        id: [9; 32],
        ..cost
    };
    assert!(runtime.with_runtime_cost(changed).is_err());
    let foreign = shards_api::NativeImages::new(
        [2; 32],
        images.cli(),
        images.kernel(),
        Some(images.vmm(false).unwrap()),
        None,
    )
    .unwrap();
    let runtime = GuestRuntime::new(Arc::new(shards_api::Shards::new()), config)
        .with_runtime_cost(cost)
        .unwrap();
    assert!(runtime.with_native_images(foreign).is_err());
    std::fs::write(&proof, b"different evidence").unwrap();
    assert!(
        super::super::load_runtime_inputs(&file, Sha256::digest(&encoded).into(), &proof).is_err()
    );
}

/// Legacy opaque evidence stays byte-identical and has no native declaration.
/// No path/default/role is guessed; malformed PRESENT prefixes refuse as a whole.
#[test]
fn native_evidence_refuses_hostile_roles_lengths_and_present_prefixes() {
    assert_eq!(parse(b"legacy opaque evidence", [1; 32]).unwrap(), None);
    let good = evidence();
    assert!(parse(&good, [1; 32]).unwrap().is_some());
    for bytes in [
        b"hex-shards-native-images".as_slice(),
        b"hex-shards-native-images 2\n",
        b"hex-shards-native-images 1\r\n",
        b"hex-shards-native-images 1\ncli none\n",
    ] {
        assert!(parse(bytes, [1; 32]).is_err());
    }
    let text = String::from_utf8_lossy(&good);
    for row in [
        "cli +3 ",
        "cli 03 ",
        "cli 0 ",
        "cli 18446744073709551616 ",
        "kernel 3 ",
        "foreign 3 ",
    ] {
        let malformed = text.replacen("cli 3 ", row, 1);
        assert!(parse(malformed.as_bytes(), [1; 32]).is_err(), "{row}");
    }
    for malformed in [
        text.replace("gpu-vmm none", "gpu-vmm none extra"),
        text.replace("cpu-vmm 2 ", "cpu-vmm none "),
        text.replace("\n\nopaque", "\nextra\nopaque"),
        text.replace("gpu-vmm none\n\n", "gpu-vmm none\n"),
    ] {
        assert!(parse(malformed.as_bytes(), [1; 32]).is_err());
    }
    let mut oversized = good;
    oversized.resize(
        hex_core::bundle::MAX_CONFIG_BYTES.checked_add(1).unwrap(),
        b'x',
    );
    assert!(parse(&oversized, [1; 32]).is_err());
}
