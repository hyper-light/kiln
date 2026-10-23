//! The container check and list (`docs/observe.md` §12.34). A node's look at its containers, on a
//! copy of a busy node's cgroup v2 hierarchy (systemd's slices and services around them; half its
//! containers Docker's, half Kubernetes' pods' of containerd, each pod's sandbox beside them; their
//! `/var/log/containers` links; each container's files as the kernel writes them): found (the
//! walk), and looked at whole (each one's files read, its tags and series made, its rates taken: a
//! second look); and the same on this machine's own hierarchy, as it is. And the server's list of a
//! fleet's containers: made of the store (each container's series, tags and newest sample, its
//! figures), and listed as the API asks (a page by name; found by a part of a name, by a tag's glob;
//! ordered by its CPU against its limit; one by the first 12 of its id), each answered as the API
//! answers it (its JSON). Time (the least of nine, and their median), allocations.
//!
//! `cargo bench -p kiln --bench containers` (`KOBS_CONTAINERS`: a node's, 1,000; `KOBS_FLEET`: the
//! fleets', `10000,50000`; `KOBS_DIR`: where the copy goes, the system's temporary folder else).
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing, clippy::panic, clippy::cast_precision_loss)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant};

use hyper_measure::alloc;
use kiln::observe::containers::inventory::{self, Inventory};
use kiln::observe::containers::{Check, cgroup, http};
use kiln_obs::batch::{Builder, Kind, SeriesKey};
use kiln_obs::hash::Keyed;
use kiln_obs::otlp::metrics::Decoded;
use kiln_obs::store::Store;

#[global_allocator]
static COUNTING: alloc::Counting = alloc::Counting;

const NOW: i64 = 1_792_324_800_000;

/// A container's `memory.stat` as Linux 6.x writes it (72 lines), its values made up.
const MEMORY_STAT_KEYS: [&str; 72] = [
    "anon",
    "file",
    "kernel",
    "kernel_stack",
    "pagetables",
    "sec_pagetables",
    "percpu",
    "sock",
    "vmalloc",
    "shmem",
    "zswap",
    "zswapped",
    "zswap_incomp",
    "file_mapped",
    "file_dirty",
    "file_writeback",
    "swapcached",
    "anon_thp",
    "file_thp",
    "shmem_thp",
    "inactive_anon",
    "active_anon",
    "inactive_file",
    "active_file",
    "unevictable",
    "slab_reclaimable",
    "slab_unreclaimable",
    "slab",
    "hugetlb",
    "workingset_refault_anon",
    "workingset_refault_file",
    "workingset_activate_anon",
    "workingset_activate_file",
    "workingset_restore_anon",
    "workingset_restore_file",
    "workingset_nodereclaim",
    "pgdemote_kswapd",
    "pgdemote_direct",
    "pgdemote_khugepaged",
    "pgdemote_proactive",
    "pgsteal_kswapd",
    "pgsteal_direct",
    "pgsteal_khugepaged",
    "pgsteal_proactive",
    "pgscan_kswapd",
    "pgscan_direct",
    "pgscan_khugepaged",
    "pgscan_proactive",
    "pgrefill",
    "pgpromote_success",
    "pgscan",
    "pgsteal",
    "pswpin",
    "pswpout",
    "pgfault",
    "pgmajfault",
    "pgactivate",
    "pgdeactivate",
    "pglazyfree",
    "pglazyfreed",
    "swpin_zero",
    "swpout_zero",
    "zswpin",
    "zswpout",
    "zswpwb",
    "thp_fault_alloc",
    "thp_collapse_alloc",
    "thp_swpout",
    "thp_swpout_fallback",
    "numa_pages_migrated",
    "numa_pte_updates",
    "numa_hint_faults",
];

fn id(n: usize, salt: u64) -> String {
    let mut x = (n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ salt;
    (0..4)
        .map(|_| {
            x = x.wrapping_mul(0xBF58_476D_1CE4_E5B9).wrapping_add(0x94D0_49BB_1331_11EB);
            format!("{x:016x}")
        })
        .collect()
}

fn file(p: &Path, text: &str) {
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, text).unwrap();
}

/// A container's files at a look's counters, its process `pid`.
fn container_files(dir: &Path, n: usize, look: u64, pid: u32) {
    let cpu = 1_000_000 * (n as u64 + 1) + look * 500_000;
    file(
        &dir.join("cpu.stat"),
        &format!(
            "usage_usec {cpu}\nuser_usec {}\nsystem_usec {}\nnice_usec 0\ncore_sched.force_idle_usec 0\nnr_periods {}\nnr_throttled {}\nthrottled_usec {}\nnr_bursts 0\nburst_usec 0\n",
            cpu / 5 * 4,
            cpu / 5,
            look * 150,
            look * (n as u64 % 7),
            look * 9_000 * (n as u64 % 7)
        ),
    );
    file(&dir.join("cpu.max"), if n.is_multiple_of(3) { "max 100000\n" } else { "50000 100000\n" });
    file(&dir.join("memory.current"), &format!("{}\n", 50_000_000 + n * 4096));
    file(&dir.join("memory.max"), if n.is_multiple_of(4) { "max\n" } else { "268435456\n" });
    let stat: String = MEMORY_STAT_KEYS.iter().enumerate().map(|(i, k)| format!("{k} {}\n", (i * 4096 + n) % 90_000_000)).collect();
    file(&dir.join("memory.stat"), &stat);
    file(&dir.join("memory.events"), &format!("low 0\nhigh 0\nmax {}\noom 0\noom_kill {}\noom_group_kill 0\nsock_throttled 0\n", look, look * u64::from(n.is_multiple_of(50))));
    file(&dir.join("io.stat"), &format!("259:0 rbytes={} wbytes={} rios=15 wios=3 dbytes=0 dios=0\n253:0 rbytes=4096 wbytes=8192 rios=1 wios=2 dbytes=0 dios=0\n", look * 4096, look * 1024));
    file(&dir.join("pids.current"), &format!("{}\n", 1 + n % 40));
    file(&dir.join("cgroup.procs"), &format!("{pid}\n"));
    for f in ["cgroup.controllers", "cgroup.events", "cgroup.type", "cpu.weight", "memory.min", "memory.low", "memory.high", "memory.swap.max", "io.max", "pids.max"] {
        file(&dir.join(f), "");
    }
}

/// A node's hierarchy: `containers` containers (half Docker's, half pods' of containerd, a sandbox
/// each pod beside its containers: its process `pause` in `proc`), 300 services, a user's session;
/// their counters at `look`.
fn node(root: &Path, links: &Path, proc: &Path, containers: usize, look: u64) -> usize {
    file(&root.join("cgroup.controllers"), "cpuset cpu io memory hugetlb pids rdma misc");
    for s in 0..300 {
        file(&root.join(format!("system.slice/svc-{s}.service/cgroup.procs")), "");
    }
    file(&root.join("user.slice/user-1000.slice/session-2.scope/cgroup.procs"), "");
    file(&root.join("init.scope/cgroup.procs"), "");
    let mut scopes = 0;
    let docker = containers / 2;
    for n in 0..docker {
        container_files(&root.join(format!("system.slice/docker-{}.scope", id(n, 1))), n, look, 0);
        scopes += 1;
    }
    let mut n = docker;
    let mut pod = 0;
    while n < containers {
        let uid = format!("{:08x}_1b2c_4d5e_8f60_{:012x}", pod, pod * 7919);
        let qos = ["burstable", "besteffort"][pod % 2];
        let slice = root.join(format!("kubepods.slice/kubepods-{qos}.slice/kubepods-{qos}-pod{uid}.slice"));
        // Its sandbox, then its containers (one to three).
        let pid = 100_000 + pod as u32;
        container_files(&slice.join(format!("cri-containerd-{}.scope", id(pod, 3))), pod, look, pid);
        file(&proc.join(format!("{pid}/comm")), "pause\n");
        scopes += 1;
        for _ in 0..1 + pod % 3 {
            if n >= containers {
                break;
            }
            let cid = id(n, 2);
            let pid = 200_000 + n as u32;
            container_files(&slice.join(format!("cri-containerd-{cid}.scope")), n, look, pid);
            file(&proc.join(format!("{pid}/comm")), "app\n");
            file(&links.join(format!("app-{pod}-7d9f_shop_app-{}-{cid}.log", n % 3)), "");
            scopes += 1;
            n += 1;
        }
        pod += 1;
    }
    scopes
}

/// The least of nine and their median: on a machine others load, the least is what the work
/// itself takes, the median what it took.
#[derive(Clone, Copy)]
struct Took(Duration, Duration);

/// What `f` takes (nine runs) and allocates (its second run).
fn measure<T>(mut f: impl FnMut() -> T) -> (Took, alloc::Counts) {
    let mut times = Vec::new();
    let mut counted = alloc::Counts::default();
    for n in 0..9 {
        let t = Instant::now();
        if n == 1 {
            alloc::begin();
        }
        std::hint::black_box(f());
        if n == 1 {
            counted = alloc::end();
        }
        times.push(t.elapsed());
    }
    times.sort();
    (Took(times[0], times[times.len() / 2]), counted)
}

fn line(what: &str, t: Took, a: &alloc::Counts, more: &str) {
    println!("{what:<60} {:>9.3} / {:>9.3}ms {:>10} allocations  {more}", t.0.as_secs_f64() * 1e3, t.1.as_secs_f64() * 1e3, a.calls());
}

/// A node's looks: found, then looked at whole (the check's second look onward, its rates taken).
fn looks(what: &str, root: &Path, links: &Path, proc: &Path) {
    let Some(v) = cgroup::version(root) else {
        println!("{what}: no cgroups");
        return;
    };
    let (t, a) = measure(|| cgroup::discover(root, v, kiln::observe::containers::CONTAINERS).len());
    let found = cgroup::discover(root, v, kiln::observe::containers::CONTAINERS).len();
    line(&format!("{what}: found (the walk)"), t, &a, &format!("{found} containers"));
    let mut check = Check::at("node-1".into(), root.to_path_buf(), links.to_path_buf(), Some(v), None).with_proc(proc.to_path_buf());
    // Into one decoding, as the owner hands it back emptied, its room kept.
    let mut d = Decoded::default();
    check.look(NOW, &mut d);
    let mut ts = NOW;
    let (t, a) = measure(|| {
        ts += 15_000;
        d.clear();
        check.look(ts, &mut d);
        d.samples.len()
    });
    line(&format!("{what}: looked at whole"), t, &a, &format!("{} series", d.samples.len()));
}

/// A fleet's store: `containers` containers on nodes of 1,000, each its 18 series as the check
/// writes them (two looks), a sixth of them stopped half an hour ago.
fn fleet(containers: usize) -> kiln_obs::machine::Shared {
    let mut s = Store::new(Keyed::with_keys(1, 2));
    const METRICS: [&str; 18] = [
        "container.cpu.usage",
        "container.cpu.user",
        "container.cpu.system",
        "container.cpu.limit",
        "container.cpu.throttled",
        "container.cpu.throttled.periods",
        "container.memory.usage",
        "container.memory.working_set",
        "container.memory.rss",
        "container.memory.cache",
        "container.memory.limit",
        "container.memory.oom_events",
        "container.io.read",
        "container.io.write",
        "container.io.read.operations",
        "container.io.write.operations",
        "container.pid.count",
        "container.uptime",
    ];
    for chunk in (0..containers).collect::<Vec<_>>().chunks(500) {
        let mut b = Builder::new();
        let mut keys = Vec::new();
        for &n in chunk {
            let (host, cid, name, pod) = (format!("node-{:04}", n / 1000), id(n, 9), format!("app-{n}"), format!("app-{n}-7d9f"));
            let ns = ["shop", "search", "payments", "web"][n % 4];
            let tags: Vec<(&str, &str)> = vec![
                ("host", &host),
                ("container_id", &cid),
                ("runtime", "containerd"),
                ("container_name", &name),
                ("image_name", "registry.example.com/shop/app"),
                ("image_tag", "2.4.1"),
                ("short_image", "app"),
                ("kube_namespace", ns),
                ("pod_name", &pod),
                ("kube_container_name", "app"),
                ("kube_qos", "burstable"),
            ];
            for m in METRICS {
                let k = SeriesKey::new(m, tags.iter().copied(), if m.ends_with("periods") || m.ends_with("oom_events") { Kind::Counter } else { Kind::Gauge }).unwrap();
                b.define(&k);
                keys.push((k, n));
            }
        }
        for (k, n) in &keys {
            let last = if n % 6 == 0 { NOW - 1_800_000 } else { NOW - 5_000 };
            let v = (n % 1000) as f64 * 1e6;
            b.run(k.fp, [(last - 15_000, v), (last, v + 1.0)]);
        }
        s.apply(&b.finish()).unwrap();
    }
    Arc::new(RwLock::new(s))
}

fn q(pairs: &[(&str, &str)]) -> HashMap<String, String> {
    pairs.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect()
}

fn main() {
    let containers: usize = std::env::var("KOBS_CONTAINERS").ok().and_then(|v| v.parse().ok()).unwrap_or(1_000);
    let fleets: Vec<usize> = std::env::var("KOBS_FLEET").ok().map_or_else(|| vec![10_000, 50_000], |v| v.split(',').filter_map(|s| s.trim().parse().ok()).collect());
    let base = std::env::var("KOBS_DIR").map_or_else(|_| std::env::temp_dir(), PathBuf::from).join(format!("kiln-bench-containers-{}", std::process::id()));
    let (root, links, proc) = (base.join("cgroup"), base.join("containers"), base.join("proc"));
    let scopes = node(&root, &links, &proc, containers, 0);
    println!("a node of {containers} containers ({scopes} scopes, sandboxes among them), 300 services, in {}", base.display());
    looks("a copy of a node's", &root, &links, &proc);
    looks("this machine's own", Path::new("/sys/fs/cgroup"), Path::new("/var/log/containers"), Path::new("/proc"));
    std::fs::remove_dir_all(&base).unwrap();
    for n in fleets {
        let store = fleet(n);
        let series = store.read().unwrap().series_count();
        println!("\na fleet of {n} containers ({series} series)");
        let (t, a) = measure(|| inventory::make(&store, NOW).containers.len());
        let inv: Inventory = inventory::make(&store, NOW);
        line("the list made of the store", t, &a, &format!("{} containers", inv.containers.len()));
        for (what, ask) in [
            ("listed: a page of 100 by name", q(&[])),
            ("listed: found by a part of a name", q(&[("filter", "app-4243")])),
            ("listed: found by a tag's glob", q(&[("filter", "kube_namespace:pay* host:node-0001")])),
            ("listed: ordered by CPU against its limit", q(&[("sort_field", "cpu_limit")])),
            ("listed: the stopped, by when last seen", q(&[("state", "stopped"), ("sort_field", "last_reported")])),
        ] {
            let (t, a) = measure(|| serde_json::to_vec(&http::list_out(&inv, &ask).unwrap()).unwrap().len());
            let (_, matched) = http::listed(&inv, &ask).unwrap();
            line(&format!("{what}, answered"), t, &a, &format!("{matched} matched"));
        }
        let short = &inv.containers[inv.containers.len() / 2].id[..12];
        let (t, a) = measure(|| http::find(&inv, short).is_some());
        line("one by the first 12 of its id", t, &a, "");
    }
}
