//! The needs model in a run (CP-T7.4; `docs/design/hex-sim.md`, "Ported from kiln's load balancer"):
//! boxes spelled in kiln's needs grammar, adapted to boxes (`hex_sim::needs`), placed on Hex's
//! recorded fleet beside Linux hosts that carry GPUs of two models, VFIO functions, block and network
//! devices. Each box asks its image's platform, its class, its room and its devices; the hard-filter
//! oracle (`spec::check_filters`) replays the log and counts every placement that broke one.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

use hex_sim::generate::NodeModel;
use hex_sim::index::{Pick, Scorer};
use hex_sim::model::{
    Class, Cluster, Device, DeviceKind, GpuLoad, Kind, ModelId, NodeId, NodeSpec, Resources,
    Tenancy, WorkloadId, WorkloadSpec,
};
use hex_sim::needs;
use hex_sim::rng::Rng;
use hex_sim::sim::{self, Config, Nemesis};
use hex_sim::spec;

const GIB: u64 = 1 << 30;
const SECOND: u64 = 1_000_000_000;

/// What the boxes ask, as people would spell them: plain boxes of each class on each platform, GPUs
/// by model and by memory, devices of each kind, KVM, and asks no node can meet (an arm64 Linux
/// image, a GPU for a Shards microVM, a VFIO function for a Firecracker microVM, a GPU on Windows,
/// which the fleet's Windows host hasn't).
const SPELLINGS: [&str; 14] = [
    "cpus=1,mem=1g,class:oci",
    "cpus=2,mem=2g,disk=20g,class:firecracker,linux/amd64",
    "cpus=1,mem=1g,class:shards,linux/arm64",
    "cpus=2,mem=4g,class:oci,windows/amd64",
    "cpus=1,mem=2g,gpus=1,class:oci,gpu.model==nvidia-geforce-rtx-5060-ti",
    "cpus=4,mem=8g,gpus=1,class:oci,gpu.memory>=40g",
    "cpus=2,mem=4g,gpus=2,class:oci,gpu.model==nvidia-l4",
    "cpus=1,mem=1g,vfio=1,class:oci",
    "cpus=1,mem=1g,block=1,block.size>=500g,class:oci",
    "cpus=1,mem=1g,net=1,kvm,class:oci",
    "cpus=1,mem=1g,class:oci,linux/arm64",
    "cpus=1,mem=1g,gpus=1,class:shards",
    "cpus=1,mem=1g,vfio=1,class:firecracker",
    "cpus=1,mem=1g,gpus=1,class:oci,windows",
];

fn device(kind: DeviceKind, size: u64) -> Device {
    Device {
        kind,
        key: None,
        available: true,
        classes: hex_sim::model::Classes::NONE.with(hex_sim::model::Class::Oci),
        model: ModelId::UNNAMED,
        size,
        load: GpuLoad::default(),
    }
}

/// Hex's fleet (its ten hosts, kiln's recorded facts) and two Linux hosts beside it: one with an
/// H100 and a VFIO function, one with two L4s, a block device and a network device; each GPU with a
/// load of its own.
fn fleet(seed: u64) -> Cluster {
    let mut nodes = NodeModel::Fleet.nodes(10);
    let mut cluster = Cluster::single_owner(Vec::new(), Vec::new());
    cluster.models = NodeModel::Fleet.models();
    let h100 = cluster.model("NVIDIA H100 80GB HBM3").unwrap();
    let l4 = cluster.model("NVIDIA L4").unwrap();
    let mut r = Rng::new(seed ^ 0x9e3d);
    let mut load = |memory: u64| GpuLoad {
        busy_per_mille: Some(u16::try_from(r.range(0, 900)).unwrap()),
        memory_used: Some(r.range(0, memory / 4)),
    };
    let mut a = NodeSpec::new(
        NodeId(10),
        Resources {
            cpu_milli: 64_000,
            mem_bytes: 512 * GIB,
            gpus: 1,
            disk_bytes: 4_000 * GIB,
        },
    );
    a.devices = vec![
        Device {
            load: load(80 * GIB),
            ..Device::gpu(h100, 80 * GIB)
        },
        device(DeviceKind::Vfio, 0),
    ];
    let mut b = NodeSpec::new(
        NodeId(11),
        Resources {
            cpu_milli: 32_000,
            mem_bytes: 256 * GIB,
            gpus: 2,
            disk_bytes: 2_000 * GIB,
        },
    );
    b.devices = vec![
        Device {
            load: load(24 * GIB),
            ..Device::gpu(l4, 24 * GIB)
        },
        Device {
            load: load(24 * GIB),
            ..Device::gpu(l4, 24 * GIB)
        },
        device(DeviceKind::Block, 1_000 * GIB),
        device(DeviceKind::Net, 0),
    ];
    nodes.push(a);
    nodes.push(b);
    cluster.nodes = nodes;
    cluster
}

/// `count` boxes drawn from [`SPELLINGS`], arriving a few milliseconds apart, each running up to a
/// second.
fn boxes(cluster: &mut Cluster, seed: u64, count: u64) {
    let mut r = Rng::new(seed);
    let mut arrives = 0;
    for id in 0..count {
        let spelled = SPELLINGS[usize::try_from(r.range(0, 13)).unwrap()];
        let n = needs::parse(spelled).unwrap();
        let ask = n.ask(cluster).unwrap();
        arrives += r.range(0, 5_000_000);
        cluster.workloads.push(WorkloadSpec {
            id: WorkloadId(id),
            need: n.need,
            kind: Kind::Plain,
            arrives,
            runs_for: Some(r.range(1, SECOND)),
            tenancy: Tenancy::default(),
            class: n.class.unwrap_or_default(),
            ask,
        });
    }
}

/// CP-T7.4, the needs model's filters in a run: do place 200 boxes of [`SPELLINGS`] on the fleet and
/// its two device hosts, for 8 seeds, clean and under the random crash nemesis, by Hex's packing
/// score; expect no placement that breaks a filter (an image on another platform, a GPU of another
/// model or with too little memory free, a device held twice or of a class that can't be given it:
/// the oracle's violation counter at 0), nothing lost or started twice, every log the serial
/// specification's, and (non-vacuity) boxes placed holding devices, and nodes refused for their
/// platform, their GPUs and their devices.
#[test]
fn boxes_land_only_where_their_platform_class_and_devices_are() {
    let (mut with_devices, mut refused, mut crashes) = (0u64, [0u64; 3], 0u64);
    for seed in 0..8 {
        let mut cluster = fleet(seed);
        boxes(&mut cluster, seed, 200);
        let base = Config {
            pick: Pick {
                scorer: Scorer::HEX,
                ..Pick::default()
            },
            ..Config::default()
        };
        let crashing = Config {
            nemesis: Nemesis::Random {
                seed: seed ^ 0xdead,
                first: 10,
                per_mille: 50,
                max_crashes: 4,
            },
            ..base
        };
        for config in [base, crashing] {
            let what = format!("seed {seed}, {:?}", config.nemesis);
            let run = sim::run(&cluster, &config).unwrap_or_else(|e| panic!("{what}: {e}"));
            spec::check_slices(&cluster, &run).unwrap_or_else(|v| panic!("{what}: {v}"));
            let end = spec::check_end(&cluster, &run);
            assert_eq!(end.lost_or_double(), 0, "{what}: {end:?}");
            let filters = spec::check_filters(&cluster, &run).unwrap();
            assert_eq!(filters.violations(), 0, "{what}: {filters:?}");
            with_devices += filters.with_devices;
            let r = run.report.refused;
            refused[0] += r.platform;
            refused[1] += r.gpu;
            refused[2] += r.device;
            crashes += run.report.crashes;
        }
    }
    assert!(with_devices > 0, "no box held a device");
    assert!(
        refused.iter().all(|&n| n > 0),
        "refusals by platform, GPU, device: {refused:?}"
    );
    assert!(crashes > 0, "the nemesis never crashed");
}

/// The fleet's GPU, by its recorded model: a box asking it lands on adacachy, the one host with it.
#[test]
fn a_gpu_by_its_recorded_model_lands_on_the_host_that_has_it() {
    let mut cluster = fleet(0);
    let n = needs::parse("cpus=1,mem=1g,gpus=1,class:oci,gpu.model==NVIDIA_GeForce_RTX_5060_Ti")
        .unwrap();
    let ask = n.ask(&mut cluster).unwrap();
    assert_eq!(ask.gpu_model, Some(hex_sim::generate::FLEET_GPU));
    cluster.workloads.push(WorkloadSpec {
        id: WorkloadId(0),
        need: n.need,
        kind: Kind::Plain,
        arrives: 0,
        runs_for: None,
        tenancy: Tenancy::default(),
        class: Class::Oci,
        ask,
    });
    let run = sim::run(&cluster, &Config::default()).unwrap();
    let placed = run.store.record(WorkloadId(0)).map(|r| r.state);
    assert_eq!(placed, Some(hex_sim::store::State::Placed(NodeId(0))));
    assert_eq!(run.store.holds(NodeId(0), WorkloadId(0)), vec![0]);
}

/// AC-T7.4.1: a Linux/arm64 Shards image is placed on macOS/HVF. The independent filter oracle
/// accepts that placement and the unsupported macOS guest remains pending.
#[test]
fn linux_shards_guest_uses_mac_hvf_and_the_independent_platform_oracle() {
    use hex_sim::model::{Arch, Ask, Os, Platform, Virt};
    let mut node = NodeSpec::new(
        NodeId(0),
        Resources {
            cpu_milli: 4_000,
            mem_bytes: 1 << 30,
            ..Resources::default()
        },
    );
    node.platform = Platform {
        os: Os::Macos,
        arch: Arch::Aarch64,
    };
    node.virt = Virt {
        hvf: true,
        ..Virt::default()
    };
    node.classes = Os::Macos.classes(node.virt);
    let workloads = [Os::Linux, Os::Macos]
        .into_iter()
        .enumerate()
        .map(|(i, os)| WorkloadSpec {
            id: WorkloadId(u64::try_from(i).unwrap()),
            need: Resources {
                cpu_milli: 1,
                ..Resources::default()
            },
            kind: Kind::Plain,
            arrives: 0,
            runs_for: None,
            tenancy: Tenancy::default(),
            class: Class::Shards,
            ask: Ask {
                os: Some(os),
                arch: Some(Arch::Aarch64),
                ..Ask::default()
            },
        })
        .collect();
    let cluster = Cluster::single_owner(vec![node], workloads);
    let run = sim::run(&cluster, &Config::default()).unwrap();
    assert_eq!(
        run.store.record(WorkloadId(0)).unwrap().state,
        hex_sim::store::State::Placed(NodeId(0))
    );
    assert!(
        run.store.record(WorkloadId(1)).is_none(),
        "unsupported guest must be refused before submission"
    );
    let filters = spec::check_filters(&cluster, &run).unwrap();
    assert_eq!(filters.placements, 1);
    assert_eq!(filters.platform_off, 0);
    assert_eq!(filters.violations(), 0);
}
