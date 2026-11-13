//! Generated clusters and workloads, from one seed. Each node runs the classes its platform can, as
//! Hex's fleet's do: Linux all three, Windows OCI containers and Shards microVMs, macOS Shards
//! microVMs alone (bench/sched/huawei_workload.md). Each workload's class and need are drawn against
//! one node, a class it runs and a need inside its capacity, so every workload fits somewhere; GPU
//! work is drawn against the host's per-device initialized eligibility and known readings, using
//! the same device predicate as placement. A platform ceiling alone cannot grant an attachment.

use crate::error::SimError;
use crate::model::{
    Arch, Class, Classes, Cluster, Device, DeviceKind, GangId, Group, GroupId, Kind, ModelId,
    NodeId, NodeSpec, Os, Overheads, Owner, OwnerId, Platform, Resources, Tenancy, Time, Virt,
    WorkloadId, WorkloadSpec, unnamed_gpus,
};
use crate::rng::Rng;

const GIB: u64 = 1 << 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Shape {
    pub nodes: u32,
    pub workloads: u64,
    /// Mean gap between arrivals; gaps are uniform on `0..=2 * mean`.
    pub mean_gap: Time,
    /// Run time is uniform on `1..=2 * mean`; `None` runs until the run ends.
    pub mean_run: Option<Time>,
    /// Per mille of nodes with GPUs.
    pub gpu_nodes_per_mille: u64,
    /// The largest share of its node a workload's need is drawn up to, per mille.
    pub max_share_per_mille: u64,
}

pub fn cluster(seed: u64, shape: &Shape) -> Result<Cluster, SimError> {
    if shape.nodes == 0 {
        return Err(SimError::Config("a cluster has at least one node"));
    }
    let mut rng = Rng::new(seed);
    // At most 64 cores and 8 GiB a core: every product below is far inside u64.
    let mut nodes = Vec::new();
    for id in 0..shape.nodes {
        let cores = 2u64
            .checked_pow(u32::try_from(rng.range(2, 6)).map_err(|_| SimError::Config("cores"))?)
            .ok_or(SimError::Config("cores"))?;
        let mem_gib = cores
            .checked_mul(rng.range(2, 8))
            .ok_or(SimError::Config("memory"))?;
        let gpus = if rng.chance(shape.gpu_nodes_per_mille) {
            u32::try_from(rng.range(1, 8)).map_err(|_| SimError::Config("gpus"))?
        } else {
            0
        };
        let cpu_milli = cores.checked_mul(1000).ok_or(SimError::Config("cores"))?;
        // Six nodes in ten Linux, two Windows, two macOS: each class on many nodes, and some nodes
        // that run only some.
        let os = match rng.range(0, 9) {
            0..6 => Os::Linux,
            6..8 => Os::Windows,
            _ => Os::Macos,
        };
        let virt = Virt {
            kvm: os == Os::Linux,
            hvf: os == Os::Macos,
            ..Virt::default()
        };
        let classes = os.classes(virt);
        nodes.push(NodeSpec {
            routes: None,
            costs: [None; crate::model::Class::COUNT],
            priors: Default::default(),
            runtime_pools: Vec::new(),
            id: NodeId(id),
            capacity: Resources {
                cpu_milli,
                mem_bytes: mem_gib.checked_mul(GIB).ok_or(SimError::Config("memory"))?,
                gpus,
                disk_bytes: 0,
            },
            // Uniform up to the node's CPU: about one node in ten starts above 90% from its
            // co-tenants alone, and placements push others past it.
            background_milli: rng.range(0, cpu_milli),
            classes,
            platform: Platform {
                os,
                arch: Arch::X86_64,
            },
            virt,
            devices: unnamed_gpus(gpus),
        });
    }
    let mut workloads = Vec::new();
    workloads
        .try_reserve_exact(
            usize::try_from(shape.workloads).map_err(|_| SimError::Config("too many workloads"))?,
        )
        .map_err(|_| SimError::Config("no memory for the workloads"))?;
    let mut arrives: Time = 0;
    let last = u64::from(shape.nodes)
        .checked_sub(1)
        .ok_or(SimError::Config("a cluster has at least one node"))?;
    for id in 0..shape.workloads {
        let at = rng.range(0, last);
        let host = nodes
            .get(usize::try_from(at).map_err(|_| SimError::Config("node index"))?)
            .ok_or(SimError::Config("node index"))?;
        let share = |rng: &mut Rng, of: u64| -> u64 {
            let most = of
                .checked_mul(shape.max_share_per_mille)
                .map_or(of, |x| x / 1000)
                .max(1);
            rng.range(1, most.min(of.max(1)))
        };
        let class = one_of(&mut rng, host.classes);
        let gpu_room = gpu_room(host, class)?;
        let wants_gpu = gpu_room > 0 && rng.chance(500);
        let need = Resources {
            cpu_milli: share(&mut rng, host.capacity.cpu_milli),
            mem_bytes: share(&mut rng, host.capacity.mem_bytes),
            gpus: if wants_gpu {
                u32::try_from(rng.range(1, u64::from(gpu_room)))
                    .map_err(|_| SimError::Config("generated GPU demand count"))?
            } else {
                0
            },
            disk_bytes: 0,
        };
        let gap = rng.range(
            0,
            shape
                .mean_gap
                .checked_mul(2)
                .ok_or(SimError::TimeOverflow)?,
        );
        arrives = arrives.checked_add(gap).ok_or(SimError::TimeOverflow)?;
        let runs_for = match shape.mean_run {
            Some(mean) => Some(rng.range(1, mean.checked_mul(2).ok_or(SimError::TimeOverflow)?)),
            None => None,
        };
        let kind = match rng.range(0, 999) {
            0..400 => Kind::Plain,
            400..650 => Kind::Build,
            650..800 => Kind::Benchmark,
            // A session drawn against a host above 90% might fit no node that admits it; keep the
            // promise that every workload fits somewhere.
            _ if hot(host) => Kind::Plain,
            _ => Kind::Session,
        };
        workloads.push(WorkloadSpec {
            id: WorkloadId(id),
            need,
            kind,
            arrives,
            runs_for,
            tenancy: Tenancy::default(),
            class,
            ask: Default::default(),
        });
    }
    Ok(Cluster::single_owner(nodes, workloads))
}

/// `cluster` with its workloads spread over `owners` owners of weight 1 to 4 and bands `0..bands`,
/// drawn from `seed`: the model-based test's multi-owner clusters.
pub fn tenants(
    mut cluster: Cluster,
    seed: u64,
    owners: u32,
    bands: u8,
) -> Result<Cluster, SimError> {
    if owners == 0 || bands == 0 {
        return Err(SimError::Config("at least one owner and one band"));
    }
    let mut rng = Rng::new(seed ^ 0x7e4a_4c15);
    cluster.owners = (0..owners)
        .map(|_| Owner {
            weight: u32::try_from(rng.range(1, 4)).unwrap_or(1),
        })
        .collect();
    let top_owner = u64::from(owners.saturating_sub(1));
    let top_band = u64::from(bands.saturating_sub(1));
    for w in &mut cluster.workloads {
        w.tenancy.owner = OwnerId(u32::try_from(rng.range(0, top_owner)).unwrap_or(0));
        w.tenancy.band = u8::try_from(rng.range(0, top_band)).unwrap_or(0);
    }
    Ok(cluster)
}

/// `cluster` with a burst (`T-T7.5.1`): `count` workloads of `owner`'s, all arriving at `at`, each of
/// 1 to `max_cores` cores and 2 GiB a core, running 1 minute to `2 * mean_run` less a minute, and
/// OCI containers, Firecracker microVMs and Shards microVMs in turn ([`by_index`]). The
/// workloads are put back in arrival order (the burst after what arrived at `at` before it) and their
/// ids made dense again.
pub fn burst(
    mut cluster: Cluster,
    seed: u64,
    owner: OwnerId,
    at: Time,
    count: u64,
    max_cores: u64,
    mean_run: Time,
) -> Result<Cluster, SimError> {
    let mut rng = Rng::new(seed ^ 0xb025_7b25);
    let minute: Time = 60_000_000_000;
    let longest = mean_run
        .checked_mul(2)
        .and_then(|x| x.checked_sub(minute))
        .ok_or(SimError::TimeOverflow)?;
    let extra = usize::try_from(count).map_err(|_| SimError::Config("burst workloads"))?;
    cluster
        .workloads
        .try_reserve_exact(extra)
        .map_err(|_| SimError::Config("no memory for the burst"))?;
    for i in 0..count {
        let cores = rng.range(1, max_cores.max(1));
        cluster.workloads.push(WorkloadSpec {
            id: WorkloadId(u64::MAX),
            need: Resources {
                cpu_milli: cores.checked_mul(1000).ok_or(SimError::Config("cores"))?,
                mem_bytes: cores
                    .checked_mul(2)
                    .and_then(|g| g.checked_mul(GIB))
                    .ok_or(SimError::Config("memory"))?,
                gpus: 0,
                disk_bytes: 0,
            },
            kind: Kind::Plain,
            class: by_index(i),
            arrives: at,
            runs_for: Some(rng.range(minute, longest)),
            tenancy: Tenancy {
                owner,
                ..Tenancy::default()
            },
            ask: Default::default(),
        });
    }
    // Stable: the burst stays after what arrived at `at` before it.
    cluster.workloads.sort_by_key(|w| w.arrives);
    for (id, w) in (0u64..).zip(cluster.workloads.iter_mut()) {
        w.id = WorkloadId(id);
    }
    Ok(cluster)
}

/// `cluster` with a burst drawn from its own workloads (`T-T7.5.1` on a recorded day): `count` workloads of
/// `owner`'s (a new owner of weight 1 if the cluster has fewer), all arriving at `at`, each taking the
/// need, run time and kind of one of the cluster's workloads chosen uniformly, with replacement. The
/// workloads are put back in arrival order and their ids made dense again, as [`burst`] does.
pub fn burst_from(
    mut cluster: Cluster,
    seed: u64,
    owner: OwnerId,
    at: Time,
    count: u64,
) -> Result<Cluster, SimError> {
    let mut rng = Rng::new(seed ^ 0xb025_f0d1);
    let n = u64::try_from(cluster.workloads.len()).map_err(|_| SimError::Config("workloads"))?;
    let last = n
        .checked_sub(1)
        .ok_or(SimError::Config("a burst drawn from no workloads"))?;
    let extra = usize::try_from(count).map_err(|_| SimError::Config("burst workloads"))?;
    let mut drawn = Vec::new();
    drawn
        .try_reserve_exact(extra)
        .map_err(|_| SimError::Config("no memory for the burst"))?;
    for _ in 0..count {
        let i = usize::try_from(rng.range(0, last)).map_err(|_| SimError::Config("workloads"))?;
        let like = cluster
            .workloads
            .get(i)
            .ok_or(SimError::Config("workload index"))?;
        drawn.push(WorkloadSpec {
            id: WorkloadId(u64::MAX),
            arrives: at,
            tenancy: Tenancy {
                owner,
                ..Tenancy::default()
            },
            ..*like
        });
    }
    while usize::try_from(owner.0).is_ok_and(|o| o >= cluster.owners.len()) {
        cluster.owners.push(Owner { weight: 1 });
    }
    cluster
        .workloads
        .try_reserve_exact(extra)
        .map_err(|_| SimError::Config("no memory for the burst"))?;
    cluster.workloads.extend(drawn);
    // Stable: the burst stays after what arrived at `at` before it.
    cluster.workloads.sort_by_key(|w| w.arrives);
    for (id, w) in (0u64..).zip(cluster.workloads.iter_mut()) {
        w.id = WorkloadId(id);
    }
    Ok(cluster)
}

/// `cluster` with `groups` disruption groups of budgets 1 to `max_budget`, each workload in one
/// of them with probability `per_mille / 1000`.
pub fn groups(
    mut cluster: Cluster,
    seed: u64,
    groups: u32,
    max_budget: u32,
    per_mille: u64,
) -> Cluster {
    let mut rng = Rng::new(seed ^ 0x9e0b_d15e);
    cluster.groups = (0..groups)
        .map(|_| Group {
            budget: u32::try_from(rng.range(1, u64::from(max_budget.max(1)))).unwrap_or(1),
        })
        .collect();
    if groups == 0 {
        return cluster;
    }
    for w in &mut cluster.workloads {
        if rng.chance(per_mille) {
            let g = rng.range(0, u64::from(groups.saturating_sub(1)));
            w.tenancy.group = Some(GroupId(u32::try_from(g).unwrap_or(0)));
        }
    }
    cluster
}

/// `cluster` with runs of 2 to `max_size` consecutive workloads made gangs (workloads that only work together),
/// starting one at a workload with probability `per_mille / 1000`: each member takes the first's
/// arrival, owner, band and group, so a gang is submitted at once into one queue.
pub fn gangs(mut cluster: Cluster, seed: u64, per_mille: u64, max_size: u64) -> Cluster {
    let mut rng = Rng::new(seed ^ 0x6a46_9a46);
    let mut next = 0u64;
    let mut at = 0usize;
    while let Some(first) = cluster.workloads.get(at).copied() {
        if !rng.chance(per_mille) {
            at = at.saturating_add(1);
            continue;
        }
        let size = usize::try_from(rng.range(2, max_size.max(2))).unwrap_or(2);
        let gang = Some(GangId(next));
        next = next.saturating_add(1);
        for w in cluster.workloads.iter_mut().skip(at).take(size) {
            w.arrives = first.arrives;
            w.tenancy = Tenancy {
                gang,
                ..first.tenancy
            };
        }
        at = at.saturating_add(size);
    }
    cluster
}

/// One tenant's workloads over a [`Day`]: their sizes, and how much of the cluster's capacity they
/// ask for, in parts per million, in the busy hours and out of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tenant {
    pub weight: u32,
    pub cores: (u64, u64),
    pub gib: (u64, u64),
    pub busy_ppm: u64,
    pub quiet_ppm: u64,
}

/// A day of workloads on identical nodes: each tenant's workloads arrive at its rate, more in the busy
/// hours `busy.0..busy.1`, and run for a minute to `2 * mean_run` less a minute.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Day {
    pub nodes: u32,
    pub node: Resources,
    pub tenants: Vec<Tenant>,
    pub length: Time,
    pub busy: (Time, Time),
    pub mean_run: Time,
    /// Past this many workloads the day is refused, not cut short.
    pub max_vms: usize,
}

/// A generated day of workloads ([`Day`]): every tenant an owner, and the workloads OCI containers,
/// Firecracker microVMs and Shards microVMs in turn by id ([`by_index`]), a third of each.
pub fn day(seed: u64, day: &Day) -> Result<Cluster, SimError> {
    if day.nodes == 0 || day.tenants.is_empty() {
        return Err(SimError::Config("a day has nodes and tenants"));
    }
    let nodes: Vec<NodeSpec> = (0..day.nodes)
        .map(|id| NodeSpec {
            routes: None,
            costs: [None; crate::model::Class::COUNT],
            priors: Default::default(),
            runtime_pools: Vec::new(),
            id: NodeId(id),
            capacity: day.node,
            background_milli: 0,
            classes: Classes::ALL,
            platform: Platform::default(),
            virt: Virt {
                kvm: true,
                ..Virt::default()
            },
            devices: unnamed_gpus(day.node.gpus),
        })
        .collect();
    let total_cpu = day
        .node
        .cpu_milli
        .checked_mul(u64::from(day.nodes))
        .ok_or(SimError::Config("cluster CPU"))?;
    let total_mem = day
        .node
        .mem_bytes
        .checked_mul(u64::from(day.nodes))
        .ok_or(SimError::Config("cluster memory"))?;
    let mut rng = Rng::new(seed);
    let mut vms: Vec<(Time, u32, Resources, Time)> = Vec::new();
    for (owner, tenant) in (0u32..).zip(&day.tenants) {
        let mut t: Time = 0;
        while t < day.length {
            if vms.len() >= day.max_vms {
                return Err(SimError::Full {
                    what: "the day's workloads",
                    bound: day.max_vms,
                });
            }
            let cores = rng.range(tenant.cores.0, tenant.cores.1);
            let need = Resources {
                cpu_milli: cores.checked_mul(1000).ok_or(SimError::Config("cores"))?,
                mem_bytes: rng
                    .range(tenant.gib.0, tenant.gib.1)
                    .checked_mul(GIB)
                    .ok_or(SimError::Config("memory"))?,
                gpus: 0,
                disk_bytes: 0,
            };
            // The workload's dominant share of the cluster, in parts per million.
            let ppm = |n: u64, of: u64| {
                n.checked_mul(1_000_000)
                    .and_then(|x| x.checked_div(of))
                    .unwrap_or(0)
            };
            let leg_ppm = ppm(need.cpu_milli, total_cpu).max(ppm(need.mem_bytes, total_mem));
            let rate = if (day.busy.0..day.busy.1).contains(&t) {
                tenant.busy_ppm
            } else {
                tenant.quiet_ppm
            };
            // Mean gap = run time * workload share / the share asked for, so the workloads in flight ask for
            // `rate` of the cluster.
            let mean_gap = day
                .mean_run
                .checked_mul(leg_ppm)
                .and_then(|x| x.checked_div(rate.max(1)))
                .ok_or(SimError::TimeOverflow)?
                .max(1);
            t = t
                .checked_add(rng.range(1, mean_gap.checked_mul(2).ok_or(SimError::TimeOverflow)?))
                .ok_or(SimError::TimeOverflow)?;
            let minute: Time = 60_000_000_000;
            let runs_for = rng.range(
                minute,
                day.mean_run
                    .checked_mul(2)
                    .and_then(|x| x.checked_sub(minute))
                    .ok_or(SimError::TimeOverflow)?,
            );
            if t < day.length {
                vms.push((t, owner, need, runs_for));
            }
        }
    }
    vms.sort_unstable_by_key(|&(t, owner, _, _)| (t, owner));
    let mut workloads = Vec::new();
    workloads
        .try_reserve_exact(vms.len())
        .map_err(|_| SimError::Config("no memory for the workloads"))?;
    for (id, (arrives, owner, need, runs_for)) in (0u64..).zip(vms) {
        workloads.push(WorkloadSpec {
            id: WorkloadId(id),
            need,
            kind: Kind::Plain,
            class: by_index(id),
            arrives,
            runs_for: Some(runs_for),
            tenancy: Tenancy {
                owner: OwnerId(owner),
                ..Tenancy::default()
            },
            ask: Default::default(),
        });
    }
    Ok(Cluster {
        nodes,
        workloads,
        models: Vec::new(),
        owners: day
            .tenants
            .iter()
            .map(|r| Owner { weight: r.weight })
            .collect(),
        groups: Vec::new(),
        overheads: Overheads::default(),
    })
}

/// G2's two node models (`bench/sched/huawei_workload.md`, "Where each kind may land"; the same hosts
/// as `bench/sched/huawei_trace.py:79-94`). Each node runs the classes its OS runs: Linux all three,
/// Windows OCI containers and Shards microVMs, macOS Shards microVMs alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeModel {
    /// EC2 m5d.metal, the main model: 48,000 millicores, 384 GiB, four 900 GB NVMe SSDs, no GPU,
    /// Linux on x86_64 with KVM, so every class (Agache et al., NSDI 2020 §6: "two Intel Xeon
    /// Platinum 8175M processors (for a total of 48 cores with hyper-threading disabled), 384GB of
    /// RAM"; AWS's M5 instance page: m5d.metal, "4 x 900 NVMe SSD").
    M5dMetal,
    /// Hex's fleet beside it: its ten hosts from kiln's runner facts (2026-11-05), in G2's order,
    /// node `id` being host `id mod 10`, as `sched_fast_path` repeats them.
    Fleet,
}

/// One of Hex's fleet's hosts: threads, memory bytes and OS as G2 recorded them (2026-11-05,
/// `huawei_trace.py:83-94`), and its architecture, disk bytes, GPU (model and memory bytes) and KVM
/// as kiln's runner facts gave them on 2026-11-09 (`ci.runners.list`). Only a GPU some class on the
/// host can be given is listed: an Apple GPU, which macOS's one class (Shards, through shards-api)
/// can't be given, is not.
struct Host {
    threads: u64,
    mem: u64,
    os: Os,
    arch: Arch,
    disk: u64,
    gpu: Option<(&'static str, u64)>,
    kvm: bool,
}

const RTX_5060_TI: (&str, u64) = ("NVIDIA GeForce RTX 5060 Ti", 8_546_942_976);

const fn host(threads: u64, mem: u64, os: Os, disk: u64) -> Host {
    Host {
        threads,
        mem,
        os,
        arch: if matches!(os, Os::Macos) {
            Arch::Aarch64
        } else {
            Arch::X86_64
        },
        disk,
        gpu: None,
        kvm: matches!(os, Os::Linux),
    }
}

/// Hex's fleet, in G2's order.
const FLEET: [Host; 10] = [
    // adacachy: the one GPU G2 counted.
    Host {
        gpu: Some(RTX_5060_TI),
        ..host(32, 131_986_903_040, Os::Linux, 7_997_298_634_752)
    },
    host(32, 67_128_643_584, Os::Linux, 7_998_785_191_936), // fedora, kiln-a094a12d.attlocal.net
    host(32, 132_766_306_304, Os::Windows, 992_549_867_520), // jupiter
    host(18, 68_719_476_736, Os::Macos, 1_995_165_736_960), // adalundhe-macbook-pro
    host(14, 38_654_705_664, Os::Macos, 494_384_795_648),   // adas-mac-studio-2.local
    host(14, 38_654_705_664, Os::Macos, 494_384_795_648),   // adas-mac-studio-3.local
    host(14, 38_654_705_664, Os::Macos, 494_384_795_648),   // adas-mac-studio.local
    host(32, 103_079_215_104, Os::Macos, 3_996_329_328_640), // adas-mac-studio.local-2
    host(15, 51_539_607_552, Os::Macos, 994_610_155_520),   // adas-macbook-pro.local
    host(18, 137_438_953_472, Os::Macos, 7_998_499_225_600), // e30189ea29182644fe5a4b2e06c7dac2
];

/// The device model the fleet's GPU is: the first a cluster of the fleet interns
/// ([`NodeModel::models`]).
pub const FLEET_GPU: ModelId = ModelId(0);

impl NodeModel {
    /// Node `id` of the model.
    pub fn node(self, id: u32) -> NodeSpec {
        match self {
            NodeModel::M5dMetal => NodeSpec {
                routes: None,
                costs: [None; crate::model::Class::COUNT],
                priors: Default::default(),
                runtime_pools: Vec::new(),
                id: NodeId(id),
                capacity: Resources {
                    cpu_milli: 48_000,
                    mem_bytes: 384 << 30,
                    gpus: 0,
                    disk_bytes: 3_600_000_000_000,
                },
                background_milli: 0,
                classes: Classes::ALL,
                platform: Platform::default(),
                virt: Virt {
                    kvm: true,
                    ..Virt::default()
                },
                devices: Vec::new(),
            },
            NodeModel::Fleet => {
                // `id mod 10` is always a host; the fallback is unreachable and empty.
                let at = usize::try_from(id.checked_rem(10).unwrap_or(0)).unwrap_or(0);
                let none = host(0, 0, Os::Macos, 0);
                let h = FLEET.get(at).unwrap_or(&none);
                let virt = Virt {
                    kvm: h.kvm,
                    nested: h.kvm,
                    hvf: matches!(h.os, Os::Macos),
                };
                let devices: Vec<Device> = h
                    .gpu
                    .map(|(_, memory)| Device::gpu(FLEET_GPU, memory))
                    .into_iter()
                    .collect();
                NodeSpec {
                    routes: None,
                    costs: [None; crate::model::Class::COUNT],
                    priors: Default::default(),
                    runtime_pools: Vec::new(),
                    id: NodeId(id),
                    capacity: Resources {
                        cpu_milli: h.threads.saturating_mul(1000),
                        mem_bytes: h.mem,
                        gpus: u32::try_from(devices.len()).unwrap_or(0),
                        disk_bytes: h.disk,
                    },
                    background_milli: 0,
                    classes: h.os.classes(virt),
                    platform: Platform {
                        os: h.os,
                        arch: h.arch,
                    },
                    virt,
                    devices,
                }
            }
        }
    }

    /// The device models the model's nodes name, in [`ModelId`] order: a cluster of its nodes
    /// interns them first.
    pub fn models(self) -> Vec<String> {
        match self {
            NodeModel::M5dMetal => Vec::new(),
            NodeModel::Fleet => vec![crate::model::model_name(RTX_5060_TI.0)],
        }
    }

    /// `n` nodes of the model, ids from 0.
    pub fn nodes(self, n: u32) -> Vec<NodeSpec> {
        (0..n).map(|id| self.node(id)).collect()
    }

    pub fn name(self) -> &'static str {
        match self {
            NodeModel::M5dMetal => "EC2 m5d.metal",
            NodeModel::Fleet => "Hex's fleet",
        }
    }
}

/// Each class's runtime overhead in bytes of memory, as G2 measured or cited them when G6 was measured
/// (`bench/sched/huawei_workload.md`, "Overheads"): OCI 440,402 (0.42 MiB, Hex's idle container,
/// `bench/competitive/RESULT.md:95`); Firecracker 5 MiB (its specification's bound on the VMM, and
/// Agache et al., NSDI 2020); Shards 5 MiB, the stand-in G2's traces have since replaced with Shards'
/// own, measured by guest size (`bench/sched/shards_overhead.md`).
pub fn overheads() -> Overheads {
    let mut o = Overheads::default();
    o.set(Class::Oci, 440_402);
    o.set(Class::Firecracker, 5 << 20);
    o.set(Class::Shards, 5 << 20);
    o
}

/// Gives each workload an owner, uniformly from `ceil(workloads / per_owner)` owners of weight 1,
/// from `seed`. G2's recorded day has 75 launches an owner (25,295 of 339 owners,
/// `bench/sched/huawei_workload.md`, "Each request's kind").
pub fn owners(cluster: &mut Cluster, seed: u64, per_owner: u64) -> Result<(), SimError> {
    let n = u64::try_from(cluster.workloads.len())
        .map_err(|_| SimError::Config("too many workloads"))?;
    let count = n
        .checked_add(per_owner.saturating_sub(1))
        .and_then(|x| x.checked_div(per_owner))
        .ok_or(SimError::Config("an owner has at least one launch"))?
        .max(1);
    let last = count.checked_sub(1).ok_or(SimError::Config("owners"))?;
    let mut rng = Rng::new(seed ^ 0x0e1e_7e45);
    for w in &mut cluster.workloads {
        w.tenancy.owner =
            OwnerId(u32::try_from(rng.range(0, last)).map_err(|_| SimError::Config("owners"))?);
    }
    cluster.owners =
        vec![Owner::default(); usize::try_from(count).map_err(|_| SimError::Config("owners"))?];
    Ok(())
}

/// G2's kind rule (`bench/sched/huawei_trace.py`'s `classes_by_owner`): all of an owner's workloads
/// are one class, and owners are split so each class gets a third of the launches: the owner with the
/// most launches first (then the lower id), each to the class with the fewest launches so far, ties
/// OCI, then Firecracker, then Shards. Each workload's memory then gains its class's overhead. Returns
/// the launches each class got, OCI, Firecracker, Shards.
pub fn by_owner(cluster: &mut Cluster, overheads: Overheads) -> Result<[u64; 3], SimError> {
    let mut launches = vec![0u64; cluster.owners.len()];
    for w in &cluster.workloads {
        let at = usize::try_from(w.tenancy.owner.0).map_err(|_| SimError::Config("owner"))?;
        let n = launches.get_mut(at).ok_or(SimError::Config(
            "a workload's owner is one of the cluster's",
        ))?;
        *n = n.checked_add(1).ok_or(SimError::Config("launches"))?;
    }
    let mut order: Vec<usize> = (0..launches.len()).collect();
    order.sort_by_key(|&o| (std::cmp::Reverse(launches.get(o).copied().unwrap_or(0)), o));
    const CLASSES: [Class; 3] = [Class::Oci, Class::Firecracker, Class::Shards];
    let mut got = [0u64; 3];
    let mut class_of = vec![Class::Oci; launches.len()];
    for o in order {
        let k = (0..3)
            .min_by_key(|&k| (got.get(k).copied().unwrap_or(u64::MAX), k))
            .ok_or(SimError::Config("classes"))?;
        let n = launches.get(o).copied().unwrap_or(0);
        let g = got.get_mut(k).ok_or(SimError::Config("classes"))?;
        *g = g.checked_add(n).ok_or(SimError::Config("launches"))?;
        if let (Some(slot), Some(&class)) = (class_of.get_mut(o), CLASSES.get(k)) {
            *slot = class;
        }
    }
    for w in &mut cluster.workloads {
        let at = usize::try_from(w.tenancy.owner.0).map_err(|_| SimError::Config("owner"))?;
        w.class = *class_of.get(at).ok_or(SimError::Config("owner"))?;
        w.need.mem_bytes = overheads
            .of(w.class, w.need.mem_bytes)
            .and_then(|bytes| w.need.mem_bytes.checked_add(bytes))
            .ok_or(SimError::Need(w.id))?;
    }
    cluster.overheads = overheads;
    Ok(got)
}

/// Count only actual eligible observations, never the class's static platform ceiling alone.
/// The same one-device predicate as Index supplies counter quality/model/size semantics;
/// this is a bound on generated demand, not another workload or identity picker.
fn gpu_room(host: &NodeSpec, class: Class) -> Result<u32, SimError> {
    if !host.classes.has(class) || !class.exposes(host.platform.os, DeviceKind::Gpu) {
        return Ok(0);
    }
    let ask = crate::model::Ask::default();
    let count = host
        .devices
        .iter()
        .filter(|device| {
            crate::model::fits_devices(
                std::slice::from_ref(*device),
                |_| device.available && device.classes.has(class),
                1,
                &ask,
            )
        })
        .count();
    u32::try_from(count)
        .map(|count| count.min(host.capacity.gpus))
        .map_err(|_| SimError::Config("generated GPU capability count"))
}

/// One of the classes `classes` holds, each alike (OCI when it holds none).
fn one_of(rng: &mut Rng, classes: Classes) -> Class {
    let pick = rng.range(0, u64::from(classes.count().saturating_sub(1)));
    [Class::Oci, Class::Firecracker, Class::Shards]
        .into_iter()
        .filter(|&c| classes.has(c))
        .nth(usize::try_from(pick).unwrap_or(0))
        .unwrap_or_default()
}

/// Above 90% CPU from its background alone: `index::HOT_PER_MILLE`, as busy * 1000 > capacity * 900.
fn hot(node: &NodeSpec) -> bool {
    let lhs = u128::from(node.background_milli).checked_mul(1000);
    let rhs =
        u128::from(node.capacity.cpu_milli).checked_mul(u128::from(crate::index::HOT_PER_MILLE));
    matches!((lhs, rhs), (Some(l), Some(r)) if l > r)
}

/// The `n`th of OCI container, Firecracker microVM and Shards microVM, cycling: a generated day's and a
/// burst's classes, by index, so they take nothing from the generator's random stream.
fn by_index(n: u64) -> Class {
    match n % 3 {
        0 => Class::Oci,
        1 => Class::Firecracker,
        _ => Class::Shards,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape() -> Shape {
        Shape {
            nodes: 50,
            workloads: 500,
            mean_gap: 1_000,
            mean_run: Some(50_000),
            gpu_nodes_per_mille: 200,
            max_share_per_mille: 500,
        }
    }

    /// T-T7.4.1's generator: one seed, one cluster; every workload fits an empty node, arrivals in
    /// order; each class drawn, some nodes running only some, and GPU work within actual eligibility.
    #[test]
    fn a_seed_names_one_cluster_of_workloads_that_fit() {
        let a = cluster(3, &shape()).unwrap();
        assert_eq!(a, cluster(3, &shape()).unwrap());
        assert_ne!(a, cluster(4, &shape()).unwrap());
        let empty = crate::index::FitIndex::new(&a.nodes).unwrap();
        for w in &a.workloads {
            assert!(empty.admitted_anywhere(&w.demand()), "{w:?}");
        }
        for kind in [Kind::Plain, Kind::Plain, Kind::Benchmark, Kind::Session] {
            assert!(a.workloads.iter().any(|w| w.kind == kind), "{kind:?}");
        }
        assert!(a.workloads.windows(2).all(|p| p[0].arrives <= p[1].arrives));
        assert!(a.workloads.iter().any(|w| w.need.gpus > 0));
        for class in [Class::Oci, Class::Firecracker, Class::Shards] {
            assert!(a.workloads.iter().any(|w| w.class == class), "{class:?}");
        }
        assert!(a.nodes.iter().any(|n| n.classes != Classes::ALL));
        assert!(a.workloads.iter().all(|w| {
            w.need.gpus == 0
                || a.nodes
                    .iter()
                    .any(|node| gpu_room(node, w.class).is_ok_and(|room| room >= w.need.gpus))
        }));
    }

    /// AC-T7.4.1: generated demand follows initialized class masks and known pressure on each slot.
    #[test]
    fn generated_gpu_room_uses_mixed_initialized_classes_and_counter_quality() {
        let mut host = NodeSpec::new(
            NodeId(0),
            Resources {
                cpu_milli: 4_000,
                mem_bytes: 4_096,
                disk_bytes: 0,
                gpus: 3,
            },
        );
        host.virt.kvm = true;
        host.classes = Classes::ALL;
        host.devices = vec![Device::gpu(ModelId::UNNAMED, 4_096); 3];
        host.devices[0].classes = Classes::NONE.with(Class::Oci);
        host.devices[1].classes = Classes::NONE.with(Class::Shards);
        host.devices[2].classes = Classes::NONE.with(Class::Firecracker);
        assert_eq!(gpu_room(&host, Class::Oci).unwrap(), 1);
        assert_eq!(gpu_room(&host, Class::Shards).unwrap(), 1);
        assert_eq!(gpu_room(&host, Class::Firecracker).unwrap(), 1);
        host.devices[2].load.memory_used = None;
        assert_eq!(gpu_room(&host, Class::Firecracker).unwrap(), 0);
        assert_eq!(
            host.capacity.gpus, 3,
            "physical count is not fresh capability"
        );
        host.devices[1].available = false;
        assert_eq!(gpu_room(&host, Class::Shards).unwrap(), 0);
        host.devices[1].available = true;
        host.devices[2].load.memory_used = Some(0);
        host.devices[2].classes = Classes::ALL;
        assert_eq!(gpu_room(&host, Class::Oci).unwrap(), 2);
        assert_eq!(gpu_room(&host, Class::Shards).unwrap(), 2);
        host.classes = Classes::NONE.with(Class::Oci);
        assert_eq!(gpu_room(&host, Class::Shards).unwrap(), 0);
    }

    /// G2's kind rule (`huawei_trace.py`'s `classes_by_owner`): do owners of 5, 3, 3, 2 and 1
    /// launches; expect OCI 5, Firecracker 5 (owners 1 and 3), Shards 4 (owners 2 and 4), each
    /// workload's memory raised by its class's overhead.
    #[test]
    fn owners_go_whole_to_the_class_with_fewest_launches() {
        let mut c = cluster(3, &shape()).unwrap();
        c.workloads.truncate(14);
        let counts = [5u32, 3, 3, 2, 1];
        let mut at = 0;
        for (o, &n) in counts.iter().enumerate() {
            for _ in 0..n {
                c.workloads[at].tenancy.owner = OwnerId(u32::try_from(o).unwrap());
                at += 1;
            }
        }
        c.owners = vec![Owner::default(); 5];
        let before: Vec<u64> = c.workloads.iter().map(|w| w.need.mem_bytes).collect();
        let got = by_owner(&mut c, overheads()).unwrap();
        assert_eq!(got, [5, 5, 4]);
        let want = [
            Class::Oci,
            Class::Firecracker,
            Class::Shards,
            Class::Firecracker,
            Class::Shards,
        ];
        for (w, mem) in c.workloads.iter().zip(before) {
            let class = want[usize::try_from(w.tenancy.owner.0).unwrap()];
            assert_eq!(w.class, class);
            assert_eq!(w.need.mem_bytes, mem + overheads().of(class, mem).unwrap());
        }
        assert_eq!(c.overheads, overheads());
    }
}
