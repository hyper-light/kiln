use super::{Input, prepare};
use crate::guest::{GuestConfig, shares::Share};
use hex_api::{CapacityResources, RuntimeCost};
use sha2::{Digest as _, Sha256};
use std::path::Path;

fn config(root: &Path) -> GuestConfig {
    GuestConfig {
        binary: root.join("bound-cli"),
        kernel: root.join("bound-kernel"),
        initrd: root.join("init"),
        vcpus: 1,
        memory_mib: 256,
        gpu: false,
        shares: vec![
            Share {
                dir: root.join("read-Ω"),
                read_only: true,
                at: "/one".into(),
            },
            Share {
                dir: root.join("write"),
                read_only: false,
                at: "/two".into(),
            },
        ],
        staged: "/run/hex/staged".into(),
    }
}

fn profile(id: [u8; 32]) -> RuntimeCost {
    // Pure model inputs. ROOT supplies actual artifacts and measured profile to
    // the native measurement; this unit launches no image, process or VM.
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

fn write_cost(path: &Path, cost: RuntimeCost) -> [u8; 32] {
    let bytes = serde_json::to_vec(&cost).unwrap();
    std::fs::write(path, &bytes).unwrap();
    Sha256::digest(&bytes).into()
}

/// AC-T7.4.1: one real evidence loader/typed plan is reusable by each fresh
/// image, without a Runtime/Prober, authority cookie or parent pointer.
#[test]
fn fixed_native_fixture_uses_real_evidence_and_refuses_legacy_or_wrong_cost() {
    let dir = tempfile::tempdir().unwrap();
    let evidence = dir.path().join("evidence");
    let cost_file = dir.path().join("cost");
    let mut bytes = b"hex-shards-native-images 1\n".to_vec();
    for (role, data) in [
        ("cli", b"cli".as_slice()),
        ("kernel", b"kernel"),
        ("cpu-vmm", b"vmm"),
    ] {
        let digest: String = Sha256::digest(data)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        bytes.extend_from_slice(format!("{role} {} {digest}\n", data.len()).as_bytes());
    }
    bytes.extend_from_slice(b"gpu-vmm none\n\nopaque model evidence\n");
    std::fs::write(&evidence, &bytes).unwrap();
    let cost = profile(Sha256::digest(&bytes).into());
    let hash = write_cost(&cost_file, cost);
    let config = config(dir.path());
    let paths =
        shards_api::RuntimePaths::new(dir.path().join("cache"), dir.path().join("home")).unwrap();
    // Syntactic locator only; the builder never creates or connects a socket.
    let socket = dir
        .path()
        .ancestors()
        .last()
        .unwrap()
        .join("fixture-model.v");
    let input = Input {
        config: &config,
        cost_file: &cost_file,
        cost_sha256: hash,
        evidence: &evidence,
        paths: &paths,
        socket: &socket,
        console_kept: 0,
    };
    let (plan, allocation) = prepare(input).unwrap();
    assert_eq!(allocation.cost, cost);
    assert_eq!(
        allocation.resources,
        CapacityResources {
            cpu_milli: 1000,
            mem_bytes: 256 << 20,
            disk_bytes: 0,
        }
    );
    let view = plan.native_startup().unwrap();
    assert_eq!(view.binary(), config.binary);
    assert_eq!(view.kernel(), config.kernel);
    assert_eq!(view.images().evidence(), cost.id);
    assert_eq!(view.paths(), &paths);
    assert_eq!(view.console_kept(), 0);
    assert_eq!(
        view.console_pin(),
        <[u8; 32]>::from(Sha256::digest(b"kernel"))
    );
    assert_eq!(view.shares().len(), 2);
    assert!(view.shares().first().unwrap().read_only);
    assert!(!view.shares().get(1).unwrap().read_only);
    let mut wrong = cost;
    wrong.floor.cpu_milli = 2000;
    wrong.ceiling.cpu_milli = 2000;
    let wrong_hash = write_cost(&cost_file, wrong);
    assert!(
        prepare(Input {
            cost_sha256: wrong_hash,
            ..input
        })
        .is_err()
    );
    std::fs::write(&evidence, b"legacy evidence, no native image declaration").unwrap();
    let legacy = profile(Sha256::digest(b"legacy evidence, no native image declaration").into());
    let legacy_hash = write_cost(&cost_file, legacy);
    assert!(
        prepare(Input {
            cost_sha256: legacy_hash,
            ..input
        })
        .is_err()
    );
}

/// HEX-GUEST-11/12: fallible construction preserves the actual kernel grammar,
/// including zero roots and mixed modes, in original order.
#[test]
fn fallible_fixture_spec_preserves_existing_kernel_and_ordered_share_grammar() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = config(dir.path());
    let spec = config.try_spec().unwrap();
    assert_eq!(spec.cmdline, "console=ttyS0 panic=-1 hex.shares=ro,rw");
    assert_eq!(spec.kernel, config.kernel);
    assert_eq!(
        spec.shares.first().unwrap().dir,
        config.shares.first().unwrap().dir
    );
    assert_eq!(
        spec.shares.get(1).unwrap().dir,
        config.shares.get(1).unwrap().dir
    );
    assert_eq!(
        spec.ports,
        [
            hex_core::stream::CONTROL_PORT,
            hex_core::stream::STDIO_PORT,
            hex_core::stream::files::FILES_PORT
        ]
    );
    config.shares.clear();
    assert_eq!(config.try_spec().unwrap().cmdline, "console=ttyS0 panic=-1");
    config.shares.resize(
        crate::guest::shares::MAX_SHARES.checked_add(1).unwrap(),
        Share {
            dir: dir.path().to_path_buf(),
            read_only: true,
            at: "/model".into(),
        },
    );
    assert!(config.try_spec().is_err());
}
