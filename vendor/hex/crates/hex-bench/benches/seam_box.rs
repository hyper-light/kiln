//! The `process` class through the hex-api `Runtime` seam, in process: box_bench.py's phases (runs of
//! `true`, throughput at concurrency 1, 8 and 32, CPU per run, and held memory and density of idle boxes)
//! with the node agent in this process, so no call spawns a process but the box's own program. It writes
//! box_bench.py's JSON under the method `hex-seam`, which summarize.py sets beside `hex box run` and
//! systemd's units in the process table; implementation goal 7 (docs/design/runtime-architecture.md,
//! decision 7) is measured on it. Kiln research 01a16eb2308472b89f1af9806ef9f32e, findings f7 and f8.
//!   seam_box <out.json> <n> <warmup> <lifecycles> <density cap> <density budget MiB>
//! (`cargo bench -p hex-bench --bench seam_box -- ...`, or its binary as host.sh's boxseam pass runs it).
//! HEXBENCH_PHASES names the phases to run, as box_bench.py's does, and HEXBENCH_CONCURRENCY the
//! throughput phase's concurrencies (1 8 32). It needs a cgroup v2 subtree with
//! every one of cpu, io, memory and pids delegated; where this process's cgroup lacks one, it re-runs
//! itself once in a `systemd-run --user --scope -p Delegate=yes`, as process_box.rs does since 460597a.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::print_stdout,
    clippy::cast_precision_loss
)]

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("SKIP: the seam bench is Linux-only");
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::main()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};
    use std::process::{Command, ExitCode};
    use std::sync::Barrier;
    use std::time::{Duration, Instant};

    use hex_api::{Bounds, BoxId, BoxSpec, ClassSpec, Exit, Proc, Program, Runtime, Stdio};
    use hex_node::Node;
    use serde_json::{Map, Value, json};

    /// Each call's bound, as box_bench.py's CALL_TIMEOUT_S.
    const CALL_TIMEOUT: Duration = Duration::from_secs(60);

    /// box_bench.py's limits for every method: 256 MiB and 64 pids.
    fn bounds() -> Bounds {
        Bounds {
            memory_max: Some(256 << 20),
            pids_max: Some(64),
            ..Bounds::default()
        }
    }

    fn spec(id: &BoxId) -> BoxSpec {
        BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: bounds(),
            class: ClassSpec::Process,
            owner: None,
            band: 0,
        }
    }

    fn own_cgroup() -> PathBuf {
        let own = std::fs::read_to_string("/proc/self/cgroup").expect("/proc/self/cgroup");
        let rel = own
            .lines()
            .find_map(|l| l.strip_prefix("0::"))
            .expect("a 0:: cgroup line")
            .trim_start_matches('/');
        PathBuf::from("/sys/fs/cgroup").join(rel)
    }

    /// Moves this process into a leaf of its own cgroup and checks that a box root there gets every
    /// controller: `BoxRoot::open` refuses by name where one is missing (f8, HEX-INST-1).
    fn prepare() -> Result<PathBuf, String> {
        let own = own_cgroup();
        let leaf = own.join("hex-agent");
        std::fs::create_dir_all(&leaf).map_err(|e| format!("making a leaf for the agent: {e}"))?;
        std::fs::write(leaf.join("cgroup.procs"), format!("{}", std::process::id()))
            .map_err(|e| format!("moving self into a leaf: {e}"))?;
        // The probe root opening proves every controller a box uses is delegated here: `BoxRoot::open`
        // refuses by name otherwise (HEX-INST-1).
        hex_runtime::linux::cgroup::BoxRoot::open(&own.join("hex-probe"), Vec::new())
            .map_err(|e| e.to_string())?;
        let _ = std::fs::remove_dir(own.join("hex-probe"));
        Ok(own)
    }

    fn read(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("reading {}: {e}", path.display()))
    }

    fn read_i64(path: &Path) -> i64 {
        read(path)
            .trim()
            .parse()
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    }

    /// The CPU the cgroup's tree has used, in microseconds (`cpu.stat` usage_usec, which the kernel
    /// counts from the scheduler's runtime, not from ticks): the agent's and its boxes'.
    fn cpu_usec(cg: &Path) -> i64 {
        read(&cg.join("cpu.stat"))
            .lines()
            .find_map(|l| l.strip_prefix("usage_usec "))
            .and_then(|v| v.trim().parse().ok())
            .expect("usage_usec in cpu.stat")
    }

    /// cgmem.py's held_bytes: memory.current less `file`, keeping `shmem`, less `zswap`, plus
    /// memory.swap.current. A process box's programs map no hugepages, so cgmem's hugetlb term is 0.
    fn held_bytes(cg: &Path) -> i64 {
        let stat: BTreeMap<String, i64> = read(&cg.join("memory.stat"))
            .lines()
            .filter_map(|l| {
                let (k, v) = l.split_once(' ')?;
                Some((k.to_owned(), v.trim().parse().ok()?))
            })
            .collect();
        read_i64(&cg.join("memory.current")) - stat["file"] + stat["shmem"]
            - stat.get("zswap").copied().unwrap_or(0)
            + read_i64(&cg.join("memory.swap.current"))
    }

    /// One run of `true` in a box made for it (decision 7): run, then wait for its end, which comes once
    /// the node agent has removed the box.
    fn run_true(node: &Node, id: &str) -> Result<(), String> {
        let id = BoxId::new(id).map_err(|e| e.to_string())?;
        let ended = node
            .run(spec(&id), Program::of(&["/usr/bin/true"]), Stdio::null())
            .map_err(|e| format!("run {id}: {e}"))?
            .wait_for(CALL_TIMEOUT)
            .map_err(|e| format!("wait in {id}: {e}"))?
            .ok_or_else(|| format!("`true` in {id} still running after {CALL_TIMEOUT:?}"))?;
        match ended {
            Exit::Code(0) => Ok(()),
            other => Err(format!("`true` in {id} ended {other:?}")),
        }
    }

    /// n timed runs after `warmup` untimed ones, each with the CPU the cgroup's tree used for it.
    fn latency(node: &Node, own: &Path, n: usize, warmup: usize) -> Result<Value, String> {
        let (mut ms, mut cpu) = (Vec::with_capacity(n), Vec::with_capacity(n));
        for i in 0..warmup + n {
            let c0 = cpu_usec(own);
            let t0 = Instant::now();
            run_true(node, &format!("l{i}"))?;
            let wall = t0.elapsed();
            let used = cpu_usec(own) - c0;
            if i >= warmup {
                ms.push(wall.as_secs_f64() * 1e3);
                cpu.push(used as f64 / 1e6);
            }
        }
        // No helper process does a call's work, so there is none to charge (box_bench.py charges
        // systemd's manager and bus here).
        Ok(json!({"ms": ms, "call_cpu_s": cpu, "helper_cpu_s_per_run": 0.0}))
    }

    /// At each concurrency, that many threads sharing the node, each running its share of the
    /// lifecycles; the clock starts once every thread is waiting at the barrier.
    fn throughput(node: &Node, lifecycles: usize) -> Result<Value, String> {
        let mut out = Map::new();
        // HEXBENCH_CONCURRENCY names the concurrencies to run (1, 8 and 32 by default): a probe of one.
        let concurrencies: Vec<usize> = std::env::var("HEXBENCH_CONCURRENCY")
            .ok()
            .filter(|c| !c.trim().is_empty())
            .map(|c| {
                c.split_whitespace()
                    .map(|n| n.parse().expect("HEXBENCH_CONCURRENCY: numbers"))
                    .collect()
            })
            .unwrap_or_else(|| vec![1, 8, 32]);
        for conc in concurrencies {
            let per = (lifecycles / conc).max(1);
            let start = Barrier::new(conc + 1);
            let (wall, errs) = std::thread::scope(|s| {
                let threads: Vec<_> = (0..conc)
                    .map(|w| {
                        let start = &start;
                        s.spawn(move || {
                            start.wait();
                            (0..per).try_for_each(|j| run_true(node, &format!("t{conc}-{w}-{j}")))
                        })
                    })
                    .collect();
                start.wait();
                let t0 = Instant::now();
                let errs: Vec<String> = threads
                    .into_iter()
                    .filter_map(|t| {
                        t.join()
                            .unwrap_or_else(|_| Err("a thread panicked".to_owned()))
                            .err()
                    })
                    .collect();
                (t0.elapsed(), errs)
            });
            if let Some(e) = errs.into_iter().next() {
                return Err(format!("concurrency {conc}: {e}"));
            }
            let total = per * conc;
            out.insert(
                conc.to_string(),
                json!({"lifecycles": total, "wall_s": wall.as_secs_f64(),
                       "per_s": total as f64 / wall.as_secs_f64()}),
            );
        }
        Ok(Value::Object(out))
    }

    /// Whether process `pid` runs `sleep`, waited for at most CALL_TIMEOUT: a run returns once the
    /// program's execve has released the agent's memory (the vfork's release, Linux v6.19
    /// fs/exec.c:854, kernel/fork.c:1488), and the kernel names it a moment later (fs/exec.c:1242-1246).
    fn runs_sleep(pid: u32) -> bool {
        let deadline = Instant::now() + CALL_TIMEOUT;
        loop {
            match std::fs::read_to_string(format!("/proc/{pid}/comm")) {
                Ok(c) if c.trim() == "sleep" => return true,
                Ok(_) if Instant::now() < deadline => std::thread::yield_now(),
                _ => return false,
            }
        }
    }

    /// How many cgroups under `cg` are removed but not yet released (`cgroup.stat` nr_dying_descendants).
    fn dying(cg: &Path) -> i64 {
        read(&cg.join("cgroup.stat"))
            .lines()
            .find_map(|l| l.strip_prefix("nr_dying_descendants "))
            .and_then(|v| v.trim().parse().ok())
            .expect("nr_dying_descendants in cgroup.stat")
    }

    /// Waits, at most CALL_TIMEOUT, until no cgroup under `cg` is dying, as cgmem.py's settle does: the
    /// kernel frees a removed cgroup's statistics, charged to its parent, only once it's released.
    /// Returns how many still were.
    fn settle(cg: &Path) -> i64 {
        let deadline = Instant::now() + CALL_TIMEOUT;
        loop {
            let left = dying(cg);
            if left == 0 || Instant::now() > deadline {
                return left;
            }
            std::thread::park_timeout(Duration::from_millis(5));
        }
    }

    /// Boxes holding `sleep infinity`, one at a time, each up before the next, until the cap or the
    /// memory budget; then what the cgroup's tree holds per box. Every box is removed afterwards, and a
    /// box or cgroup left behind is the phase's error.
    fn density(
        node: &Node,
        own: &Path,
        root: &Path,
        cap: usize,
        budget_mib: i64,
    ) -> Result<Value, String> {
        let dying_at_start = settle(own);
        let (base, held0) = (read_i64(&own.join("memory.current")), held_bytes(own));
        let (mut ids, mut procs): (Vec<BoxId>, Vec<Box<dyn Proc>>) = (Vec::new(), Vec::new());
        let mut stop = "cap".to_owned();
        let t0 = Instant::now();
        for i in 0..cap {
            let id = BoxId::new(&format!("d{i}")).map_err(|e| e.to_string())?;
            match node.run(
                spec(&id),
                Program::of(&["/usr/bin/sleep", "infinity"]),
                Stdio::null(),
            ) {
                Ok(p) => {
                    ids.push(id);
                    let up = runs_sleep(p.pid());
                    procs.push(p);
                    if !up {
                        stop = format!("box d{i}: its program isn't `sleep` once run returned");
                        break;
                    }
                }
                Err(e) => {
                    stop = format!("run d{i}: {e}");
                    break;
                }
            }
            if read_i64(&own.join("memory.current")) - base > budget_mib << 20 {
                stop = "memory budget".to_owned();
                break;
            }
        }
        let wall = t0.elapsed();
        let used = read_i64(&own.join("memory.current")) - base;
        let held_rise = held_bytes(own) - held0;
        let count = procs.len();
        for id in &ids {
            node.delete(id).map_err(|e| format!("delete {id}: {e}"))?;
        }
        // Each `sleep`'s end, which its box's delete reaped.
        for p in &mut procs {
            p.wait_for(CALL_TIMEOUT)
                .map_err(|e| format!("reaping box {}'s sleep: {e}", p.pid()))?
                .ok_or_else(|| format!("box {}'s sleep outlived its box's delete", p.pid()))?;
        }
        let boxes_left = node.list().map_err(|e| e.to_string())?.len();
        // The node's own root for oci boxes (`.oci`, since 32db274) is no box.
        let cgroups_left = std::fs::read_dir(root)
            .map_err(|e| format!("listing {}: {e}", root.display()))?
            .filter(|e| {
                e.as_ref()
                    .is_ok_and(|e| e.path().is_dir() && e.file_name() != hex_node::OCI_BOXES)
            })
            .count();
        if boxes_left > 0 || cgroups_left > 0 {
            return Err(format!(
                "after removing its {} boxes the node lists {boxes_left} and {} holds {cgroups_left} cgroups",
                ids.len(),
                root.display()
            ));
        }
        let held = json!({
            "bytes": held_rise,
            "per_unit_bytes": (count > 0 && held_rise > 0).then(|| held_rise as f64 / count as f64),
            "per_gib": (held_rise > 0).then(|| count as f64 / (held_rise as f64 / f64::from(1u32 << 30))),
        });
        Ok(
            json!({"count": count, "stop": stop, "start_wall_s": wall.as_secs_f64(), "memory_bytes": used,
                  "dying_at_start": dying_at_start, "watched": [own], "held": held}),
        )
    }

    /// Re-runs this bench in a delegated scope of the user's systemd; its status is this run's.
    fn rerun_delegated(args: &[String], why: &str) -> ExitCode {
        let me = std::env::current_exe().expect("current exe");
        let status = Command::new("systemd-run")
            .args(["--user", "--scope", "--quiet", "-p", "Delegate=yes"])
            .arg(&me)
            .args(args)
            .env("HEX_DELEGATED", "1")
            .status()
            .unwrap_or_else(|e| panic!("no delegated cgroup ({why}), and systemd-run: {e}"));
        if status.success() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    }

    pub(super) fn main() -> ExitCode {
        // `cargo bench` passes `--bench` to a harness-less bench.
        let args: Vec<String> = std::env::args_os()
            .skip(1)
            .map(|a| {
                a.into_string()
                    .unwrap_or_else(|a| panic!("argument {a:?} isn't Unicode"))
            })
            .filter(|a| a != "--bench")
            .collect();
        let [out, n, warmup, lifecycles, cap, budget] = args.as_slice() else {
            panic!(
                "usage: seam_box <out.json> <n> <warmup> <lifecycles> <density cap> <density budget MiB>"
            );
        };
        let (n, warmup, lifecycles, cap): (usize, usize, usize, usize) = (
            n.parse().unwrap(),
            warmup.parse().unwrap(),
            lifecycles.parse().unwrap(),
            cap.parse().unwrap(),
        );
        let budget: i64 = budget.parse().unwrap();
        let own = match prepare() {
            Ok(own) => own,
            Err(e) if std::env::var_os("HEX_DELEGATED").is_none() => {
                return rerun_delegated(&args, &e);
            }
            Err(e) => panic!("no usable cgroup delegation, even in a delegated scope: {e}"),
        };
        let cache =
            PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/hex-t7/seambench");
        std::fs::create_dir_all(&cache).expect("the state's cache dir");
        let state = tempfile::Builder::new()
            .prefix("seam-")
            .tempdir_in(&cache)
            .expect("a state dir");
        // The node agent's records are durable: a run of `true` writes three, each fsynced with its
        // directory, and removes one with a directory fsync, seven in all (goal 1 drops them). So the
        // filesystem under the state decides much of a run's time, and the result names it.
        let state_fs = Command::new("stat")
            .args(["-f", "-c", "%T"])
            .arg(state.path())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .expect("stat -f of the state dir");
        let root = own.join("hex-boxes-seam");
        let node = Node::open(&root, state.path(), state.path()).expect("the node agent");
        let wanted = std::env::var("HEXBENCH_PHASES")
            .ok()
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| "latency throughput density".to_owned());
        let (mut phases, mut errors) = (Map::new(), Map::new());
        // Density first, on a tree no phase has churned: a removed cgroup's kernel memory, charged to its
        // parent, is freed after it goes, so a reading taken right after thousands of boxes came and went
        // starts high and the rise comes out low (171 KiB a box after throughput against 256-285 KiB
        // alone, adacachy, 2026-10-24).
        for phase in ["density", "latency", "throughput"] {
            if !wanted.split_whitespace().any(|w| w == phase) {
                continue;
            }
            let t0 = Instant::now();
            let r = match phase {
                "latency" => latency(&node, &own, n, warmup),
                "throughput" => throughput(&node, lifecycles),
                _ => density(&node, &own, &root, cap, budget),
            };
            let note = r
                .as_ref()
                .err()
                .map(|e| format!(" ERROR {e}"))
                .unwrap_or_default();
            println!("hex-seam {phase}: {:.0}s{note}", t0.elapsed().as_secs_f64());
            match r {
                Ok(v) => phases.insert(phase.to_owned(), v),
                Err(e) => errors.insert(phase.to_owned(), Value::String(e)),
            };
        }
        drop(node);
        let _ = std::fs::remove_dir(&root);
        let result = json!({"n": n, "warmup": warmup,
                            "methods": {"hex-seam": {"phases": phases, "errors": errors,
                                                     "cgroup": own, "state": {"dir": state.path(), "fs": state_fs}}}});
        std::fs::write(out, result.to_string()).expect("writing the result");
        ExitCode::SUCCESS
    }
}
