//! The `oci` class through the hex-api `Runtime` seam, by either of its two drivers: in process (the
//! node agent in this process, `HEXBENCH_DRIVER` unset or `inprocess`) or over the node agent's local
//! socket (`HEXBENCH_DRIVER=socket`: a `hex node` process this starts, reached through `hex-client`).
//! oci_bench.py's phases, as it runs them:
//! - `lifecycle`: create, start, exec of `true` and delete of a `sleep` container, each timed alone;
//! - `lifecycle_nonet`: the same on a bundle without a network namespace of its own, so create and
//!   delete are timed with and without one (decision 4);
//! - `start_true`: create+start of a `true` container, timed together and deleted untimed (AC-T7.1.3's
//!   measure, goal 5's create+start of `true`);
//! - `throughput`: create+start+delete of a `true` container at concurrency 1, 8 and 32;
//! - `sustained`: the throughput lifecycle at concurrency 32 for `HEXBENCH_SUSTAINED` of them
//!   (10,000), its rate in each window of 1,000 and the last window's against the first's;
//! - `interleaved`, run only when `HEXBENCH_PHASES` names it: concurrency 1 and 32 in turn,
//!   pairs of turns until the ratio is resolved against the 8x bar, 10 to 40 pairs (the runs of record before
//!   Ada's 2026-11-06 rule ran a fixed 10), 500 lifecycles at 1 then 4,000 at 32 in each, so the two
//!   meet the host as it is at that moment, its load included: each pair's rates and their ratio, and the
//!   ratio of the pooled rates (the lead, 2026-11-03T06:01Z, for goal 5 extended step 6's second
//!   criterion). `HEXBENCH_INTERLEAVED_CPUS1` and `HEXBENCH_INTERLEAVED_CPUS32` (CPU lists, `4-15`) pin
//!   each half to its CPUs (the lead, 2026-11-03T17:48Z: concurrency 1 on the P-cores, 32 on all): at each
//!   switch every thread of this process and of its descendants (the socket driver's node agent, the
//!   agent's create seeds, the boxes' inits) is pinned, and each task's `Cpus_allowed_list` read back
//!   and recorded with the pair;
//! - `rtt`: inspect of a running box, timed alone, whose work is the same either way, so the socket's
//!   cost per call is the two drivers' difference in one run;
//! - `serve`, run only when `HEXBENCH_PHASES` names it: bursts of the throughput lifecycle on request, a line on
//!   stdin each, every lifecycle's create and delete timed and replied as a line on stdout, so a coordinator
//!   (bench/competitive/probes/rounds.py) interleaves this node with others round by round (goal 17's record).
//!   With `HEXBENCH_ROOT=classic` (rounds.py's hex:classic, goal 17's baseline), the in-process node builds every
//!   root the classic way (hex-node's test control `Node::with_classic_roots`); each phase's `roots` then counts
//!   its creates made so and those made pruned.
//!
//! Around every phase it reads the node's network namespace counts (made ahead, taken from its pool,
//! made on a create's path because the pool was dry or the node keeps none, discarded, failed), in
//! process through `Node::netns` and over the socket through `Client::netns` (the socket's version 2),
//! and records what the phase added: the sustained phase's made-on-the-path count is how often its
//! creates found the pool dry (decision 4's falsifier). In process it reads the node's create seeds'
//! counts the same way (`Node::create_seeds`; decision 5): the creates that waited for a seed, the
//! seeds started, the most busy at once, which the seeds' bound is judged by. No call spawns a process
//! but the container's own init and exec helper. It writes oci_bench.py's JSON under the runtime `hex-seam` (or
//! `hex-seam-socket`), which summarize.py sets beside libcrun (the in-process rival), so runtime design
//! goal 3's targets (exec p50 at most a quarter of libcrun's, start p50 within 10% of libcrun's,
//! argument A3) and goal 4's (create p50 with a network namespace at most 5% over without one) are read
//! from it. Decisions 3 and 4 of docs/design/runtime-architecture.md.
//!
//!   seam_oci <out.json> <n> <warmup> <lifecycles> <hex-binary> <sleep-bundle> <true-bundle> [<sleep-nonet-bundle>]
//! (`cargo bench -p hex-bench --bench seam_oci -- ...`, or its binary as host.sh's ociseam pass runs
//! it). HEXBENCH_PHASES names the phases to run (all six unless set); `lifecycle_nonet` needs the
//! eighth argument, a bundle like the sleep bundle without a network namespace, and records an error
//! naming it when it isn't given. The `oci` class needs root (a container's cgroup
//! device program is an eBPF program, and its namespaces are made without a user namespace), so this
//! runs in the OCI pass's privileged container, where the cgroup root is vacated to a leaf and its
//! controllers handed down, as oci_inside.sh leaves it; off that it refuses, naming why.
//! In process it reads the node's ipc and uts namespace counts the same way (`Node::ipc_uts`, goal 5 extended
//! step 4).
//! HEXBENCH_TRACE turns on the breakdown probes' markers, around the ops HEXBENCH_TRACE_OPS
//! names (`Trace`, bench/competitive/probes/exec_breakdown.md and create_breakdown.md).
//! Each parked init's proc is made with it on a root node (decision 8, parked procs), in process and in the socket
//! driver's `hex node`, with no knob to turn it off: Ada's 2026-11-06 rule removed HEXBENCH_PARKED_PROCS once
//! bench/competitive/probes/parked_procs_ab.md decided them on. In process the phase's `parked` counts say how many were made, taken by their create (hits),
//! left (misses), wanted with none made (dry) or failed; the socket driver ends its node with SIGTERM and records
//! the line of counts it says then as the result's `parked_at_exit` (made, hits, misses, dry, capped, alive), the
//! run's own node's.
//! HEXBENCH_OWNER names one owner for every box the run makes (goal 5 extended, step 6): a box's cgroup
//! is handed to the owner's next box when it reads clean, so its bundles' processes must hold no
//! CAP_SYS_ADMIN (oci_inside.sh gives them Docker's default capabilities when it's set). In process it
//! reads, around every phase as it reads the seeds', what the node's idle cgroups did (handed on, fresh,
//! missed, refused by reason, evicted) and how its deletes ended their boxes (through the init's pidfd,
//! that falling back to `cgroup.kill`, or `cgroup.kill`). HEXBENCH_REUSE=0 has that owner opt out of reuse,
//! its cgroups' and network namespaces' alike, its bundles as they are with reuse on.
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
    eprintln!("SKIP: the oci seam bench is Linux-only");
}

#[cfg(target_os = "linux")]
fn main() -> std::process::ExitCode {
    linux::main()
}

#[cfg(target_os = "linux")]
mod linux {
    use std::fs::{File, OpenOptions};
    use std::io::{BufRead, BufReader, Write};
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, ExitCode, Stdio as Std};
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    use hex_api::{
        Bounds, BoxId, BoxOwner, BoxSpec, ClassSpec, Exit, Program, Runtime, Signal, Stdio,
    };
    use hex_client::Client;
    use hex_core::{netns, seeds};
    use hex_node::Node;
    use serde_json::{Map, Value, json};

    /// Each call's bound, as oci_bench.py's CALL_TIMEOUT_S.
    const CALL_TIMEOUT: Duration = Duration::from_secs(60);

    /// The sustained phase's concurrency and the lifecycles in each of its windows, as oci_bench.py's.
    const SUSTAINED_CONCURRENCY: usize = 32;
    const SUSTAINED_WINDOW: usize = 1_000;
    /// The sustained phase's lifecycles unless `HEXBENCH_SUSTAINED` says, as oci_bench.py's.
    const SUSTAINED_DEFAULT: usize = 10_000;
    /// Each interleaved pair's lifecycles at concurrency 1 and at 32: about a second of each at the rates of runs 1
    /// to 5 of reuse_pass.md.
    const INTERLEAVED_C1: usize = 500;
    const INTERLEAVED_C32: usize = 4_000;
    /// The bar the interleaved phase's ratio is read against: concurrency 32 at 8x concurrency 1 a minimum (goal 5
    /// extended step 6's second criterion, Ada via the lead, 2026-11-03; reuse_pass.md, run 6).
    const INTERLEAVED_BAR: f64 = 8.0;
    /// A round runs pairs until the mean of their ratios is resolved against [`INTERLEAVED_BAR`]: the bar outside the mean's two-sided 95% interval, `z` the normal
    /// quantile 1.96, its standard error the pairs' over the square root of their count. A fixed count left
    /// rounds unresolved: step 6 alone read 7.80x (SE 0.41) over 20 pairs (reuse_pass.md, 20261103T170834Z),
    /// over the socket, the bar inside its interval. Ada's 2026-11-06 rule; it takes effect for runs after it. The fixed
    /// count's knob is gone with it (Ada's 2026-11-06 rule on legacy paths): the runs of record that ran 10 pairs stay
    /// records of the build that made them.
    const RESOLVED_Z: f64 = 1.96;
    /// The fewest pairs a resolved round holds: the fewest a record pooled (reuse_pass.md, run 6's socket row of
    /// 20261103T064217Z, 10 pairs), so the standard error is never read from fewer than a record was.
    const INTERLEAVED_FEWEST: usize = 10;
    /// The most pairs a round runs before it stops unresolved, and says so: the most a record pooled for one binary
    /// (reuse_pass.md, runs 9, 11 and 12, 40 pairs each). Past it no record shows what more pairs resolve.
    const INTERLEAVED_MOST: usize = 40;

    /// Whether the mean of `ratios` is resolved against [`INTERLEAVED_BAR`]: the bar outside its two-sided 95%
    /// interval.
    fn resolved(ratios: &[f64]) -> bool {
        let n = ratios.len() as f64;
        if ratios.len() < 2 {
            return false;
        }
        let mean = ratios.iter().sum::<f64>() / n;
        let var = ratios.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / (n - 1.0);
        (mean - INTERLEAVED_BAR).abs() > RESOLVED_Z * (var / n).sqrt()
    }

    /// oci_bench.py's bundles bound every container to nothing of their own; the node bounds each box
    /// the way the competitive pass's do, so the comparison is like for like: the kernel's floor plus
    /// the workload, not an unbounded cgroup: 256 MiB, the bar's bound. Goal 16's lever 4 (boxes with no memory
    /// bound) was recorded negative (docs/research/runtime/goal16-teardown.md), so its knob is gone (Ada's 2026-11-06
    /// rule on legacy paths).
    fn bounds() -> Bounds {
        Bounds {
            memory_max: Some(256 << 20),
            pids_max: Some(256),
            ..Bounds::default()
        }
    }

    /// The owner HEXBENCH_OWNER names, every box's, or none.
    static OWNER: std::sync::OnceLock<Option<BoxOwner>> = std::sync::OnceLock::new();

    fn spec(id: &BoxId, bundle: &Path) -> BoxSpec {
        BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: bounds(),
            class: ClassSpec::Bundle(bundle.to_path_buf()),
            owner: OWNER.get().cloned().flatten(),
            band: 0,
        }
    }

    /// The cgroup the box root is made under: the OCI pass's container has its cgroup root vacated to a
    /// leaf and its controllers handed down there (oci_inside.sh), so the box root is a child of the
    /// root, beside the runtimes' own container cgroups. `HEXBENCH_CGROUP` overrides it, as oci_bench.py
    /// takes it, for the driver's control-flow check off the bench host.
    fn box_root_parent() -> PathBuf {
        std::env::var_os("HEXBENCH_CGROUP")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/sys/fs/cgroup"))
    }

    /// The CPU the cgroup's tree has used, in microseconds (`cpu.stat` usage_usec).
    fn cpu_usec(cg: &Path) -> i64 {
        std::fs::read_to_string(cg.join("cpu.stat"))
            .unwrap_or_default()
            .lines()
            .find_map(|l| l.strip_prefix("usage_usec "))
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0)
    }

    /// The breakdown probes' markers (bench/competitive/probes/exec_breakdown.md, create_breakdown.md).
    /// With `HEXBENCH_TRACE` naming an ftrace instance's directory, the lifecycle phase brackets the
    /// ops `HEXBENCH_TRACE_OPS` names (`create`, `start`, `exec`; start and exec unless it says) of
    /// every other timed lifecycle in the instance's buffer, `hexbd B <op> <id>` before the call and
    /// `hexbd E <op> <id> <ns>` after it, `<ns>` the call's time as this driver measured it, and the
    /// instance records only from one to the other: its trace is those calls and what they ran, and
    /// nothing of the ops it doesn't name or of delete. The lifecycles between run untraced in the same
    /// process, so the traced calls are set against calls that met the host as they did. Each marker
    /// is written outside the time it brackets.
    struct Trace {
        on: File,
        marker: File,
        ops: Vec<String>,
    }

    impl Trace {
        fn from_env() -> Option<Self> {
            let dir = PathBuf::from(std::env::var_os("HEXBENCH_TRACE")?);
            let open = |name: &str| {
                let path = dir.join(name);
                OpenOptions::new()
                    .write(true)
                    .open(&path)
                    .unwrap_or_else(|e| panic!("HEXBENCH_TRACE: {}: {e}", path.display()))
            };
            let ops: Vec<String> = std::env::var("HEXBENCH_TRACE_OPS")
                .ok()
                .filter(|o| !o.trim().is_empty())
                .unwrap_or_else(|| "start exec".to_owned())
                .split_whitespace()
                .map(str::to_owned)
                .collect();
            if let Some(other) = ops
                .iter()
                .find(|o| !["create", "start", "exec"].contains(&o.as_str()))
            {
                panic!("HEXBENCH_TRACE_OPS: {other:?} isn't create, start or exec");
            }
            Some(Self {
                on: open("tracing_on"),
                marker: open("trace_marker"),
                ops,
            })
        }

        /// Whether calls of `op` are bracketed.
        fn brackets(&self, op: &str) -> bool {
            self.ops.iter().any(|o| o == op)
        }

        /// Starts the instance recording, then marks the call's beginning.
        fn begin(&self, op: &str, id: &str) -> Result<(), String> {
            (&self.on)
                .write_all(b"1")
                .map_err(|e| format!("tracing_on: {e}"))?;
            (&self.marker)
                .write_all(format!("hexbd B {op} {id}").as_bytes())
                .map_err(|e| format!("trace_marker: {e}"))
        }

        /// Marks the call's end with its time, then stops the instance recording.
        fn end(&self, op: &str, id: &str, took: Duration) -> Result<(), String> {
            (&self.marker)
                .write_all(format!("hexbd E {op} {id} {}", took.as_nanos()).as_bytes())
                .map_err(|e| format!("trace_marker: {e}"))?;
            (&self.on)
                .write_all(b"0")
                .map_err(|e| format!("tracing_on: {e}"))
        }
    }

    /// One container's create, start, exec of `true` and delete, each timed alone; a slot for the op
    /// that failed records the error. The init is the bundle's `sleep infinity`, so exec runs beside it.
    /// A timed op `trace` brackets is bracketed by its markers when it's on.
    fn lifecycle_once(
        node: &dyn Runtime,
        bundle: &Path,
        id: &str,
        ms: &mut Ops,
        timed: bool,
        trace: Option<&Trace>,
    ) -> Result<(), String> {
        let traced = |op: &str| trace.filter(|t| timed && t.brackets(op));
        let bid = BoxId::new(id).map_err(|e| e.to_string())?;
        traced("create").map_or(Ok(()), |t| t.begin("create", id))?;
        let create = Instant::now();
        node.create(spec(&bid, bundle))
            .map_err(|e| format!("create {id}: {e}"))?;
        let create = create.elapsed();
        traced("create").map_or(Ok(()), |t| t.end("create", id, create))?;
        // start and exec, timed; delete runs whether or not they did, so a box is never left behind.
        let inner = (|| -> Result<(Duration, Duration), String> {
            traced("start").map_or(Ok(()), |t| t.begin("start", id))?;
            let start = Instant::now();
            node.start(&bid).map_err(|e| format!("start {id}: {e}"))?;
            let start = start.elapsed();
            traced("start").map_or(Ok(()), |t| t.end("start", id, start))?;
            traced("exec").map_or(Ok(()), |t| t.begin("exec", id))?;
            let exec = Instant::now();
            let mut p = node
                .exec(&bid, Program::of(&["/usr/bin/true"]), Stdio::null())
                .map_err(|e| format!("exec in {id}: {e}"))?;
            let ended = p
                .wait_for(CALL_TIMEOUT)
                .map_err(|e| format!("wait in {id}: {e}"))?
                .ok_or_else(|| format!("`true` in {id} still running after {CALL_TIMEOUT:?}"))?;
            let exec = exec.elapsed();
            traced("exec").map_or(Ok(()), |t| t.end("exec", id, exec))?;
            if ended != Exit::Code(0) {
                return Err(format!("`true` in {id} ended {ended:?}"));
            }
            Ok((start, exec))
        })();
        let delete = Instant::now();
        node.delete(&bid).map_err(|e| format!("delete {id}: {e}"))?;
        let delete = delete.elapsed();
        let (start, exec) = inner?;
        if timed {
            ms.create.push(create.as_secs_f64() * 1e3);
            ms.start.push(start.as_secs_f64() * 1e3);
            ms.exec.push(exec.as_secs_f64() * 1e3);
            ms.delete.push(delete.as_secs_f64() * 1e3);
        }
        Ok(())
    }

    #[derive(Default)]
    struct Ops {
        create: Vec<f64>,
        start: Vec<f64>,
        exec: Vec<f64>,
        delete: Vec<f64>,
    }

    /// n timed lifecycles after `warmup` untimed ones, each operation timed alone, its boxes named
    /// `tag` and their number.
    fn lifecycle(
        node: &dyn Runtime,
        bundle: &Path,
        n: usize,
        warmup: usize,
        tag: &str,
        trace: Option<&Trace>,
    ) -> Result<Value, String> {
        let mut ms = Ops::default();
        for i in 0..warmup + n {
            lifecycle_once(
                node,
                bundle,
                &format!("{tag}{i:06}"),
                &mut ms,
                i >= warmup,
                trace.filter(|_| i % 2 == warmup % 2),
            )?;
        }
        Ok(
            json!({"ms": {"create": ms.create, "start": ms.start, "exec": ms.exec, "delete": ms.delete}}),
        )
    }

    /// The concurrencies the throughput phase runs at: oci_bench.py's 1, 8 and 32 unless
    /// `HEXBENCH_THROUGHPUT_CONC` names others (space-, tab- or comma-separated, each above zero).
    /// Goal 5 plan step 2's AC-e sets it to `8` to trace the root switch at exactly eight creates in
    /// flight; an empty or unparsable value falls back to the three.
    fn throughput_concurrency() -> Vec<usize> {
        std::env::var("HEXBENCH_THROUGHPUT_CONC")
            .ok()
            .map(|s| {
                s.split([' ', '\t', ','])
                    .filter_map(|t| t.trim().parse::<usize>().ok())
                    .filter(|&c| c > 0)
                    .collect::<Vec<usize>>()
            })
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec![1, 8, 32])
    }

    /// create+start+delete of a `true` container, at each concurrency that many threads share the
    /// node, each its share of the lifecycles; the clock starts once every thread is at the barrier.
    /// `own` is the subtree the agent and its boxes share, whose cpu.stat the per-lifecycle CPU is.
    /// The concurrencies are `throughput_concurrency`'s (1, 8 and 32 by default).
    fn throughput(
        node: &dyn Runtime,
        bundle: &Path,
        own: &Path,
        lifecycles: usize,
    ) -> Result<Value, String> {
        let mut out = Map::new();
        for conc in throughput_concurrency() {
            let per = (lifecycles / conc).max(1);
            let start = Barrier::new(conc + 1);
            let (wall, cpu_s, errs, create_ms, delete_ms) = {
                std::thread::scope(|s| {
                    let threads: Vec<_> = (0..conc)
                        .map(|w| {
                            let start = &start;
                            s.spawn(move || {
                                start.wait();
                                let mut create_ms = Vec::with_capacity(per);
                                let mut delete_ms = Vec::with_capacity(per);
                                for j in 0..per {
                                    let (c, d) = run_true_container_timed(
                                        node,
                                        bundle,
                                        &format!("t{conc}-{w}-{j:06}"),
                                    )?;
                                    create_ms.push(c.as_secs_f64() * 1e3);
                                    delete_ms.push(d.as_secs_f64() * 1e3);
                                }
                                Ok::<_, String>((create_ms, delete_ms))
                            })
                        })
                        .collect();
                    start.wait();
                    let (c0, t0) = (cpu_usec(own), Instant::now());
                    let (mut errs, mut create_ms, mut delete_ms) =
                        (Vec::new(), Vec::new(), Vec::new());
                    for t in threads {
                        match t
                            .join()
                            .unwrap_or_else(|_| Err("a thread panicked".to_owned()))
                        {
                            Ok((c, d)) => {
                                create_ms.extend(c);
                                delete_ms.extend(d);
                            }
                            Err(e) => errs.push(e),
                        }
                    }
                    (
                        t0.elapsed(),
                        (cpu_usec(own) - c0) as f64 / 1e6,
                        errs,
                        create_ms,
                        delete_ms,
                    )
                })
            };
            if let Some(e) = errs.into_iter().next() {
                return Err(format!("concurrency {conc}: {e}"));
            }
            let total = per * conc;
            out.insert(
                conc.to_string(),
                json!({"lifecycles": total, "wall_s": wall.as_secs_f64(), "create_ms": create_ms,
                       "delete_ms": delete_ms,
                       "per_s": total as f64 / wall.as_secs_f64(),
                       "cpu_s_per_lifecycle": cpu_s / total as f64}),
            );
        }
        Ok(Value::Object(out))
    }

    /// oci_bench.py's sustained phase: the throughput lifecycle at concurrency 32 for `lifecycles` of
    /// them, each thread taking the next until none are left or one has failed, the clock started once
    /// every thread is at the barrier; then the rate in each window of 1,000 by the lifecycles' ends
    /// (from the end of the window before it, or the start), and the last window's against the first's.
    /// A short burst can't show decay under churn, which one production system saw fall from 110 to 45
    /// starts/s over an hour (docs/research/runtime/oci-runtimes-and-fast-start.md §4.4).
    fn sustained(
        node: &dyn Runtime,
        bundle: &Path,
        own: &Path,
        lifecycles: usize,
    ) -> Result<Value, String> {
        let (next, failed) = (AtomicUsize::new(0), AtomicBool::new(false));
        let start = Barrier::new(SUSTAINED_CONCURRENCY + 1);
        let (t0, wall, cpu_s, ends, errs) = std::thread::scope(|s| {
            let threads: Vec<_> = (0..SUSTAINED_CONCURRENCY)
                .map(|_| {
                    s.spawn(|| {
                        start.wait();
                        let mut ends = Vec::new();
                        while !failed.load(Ordering::Relaxed) {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            if i >= lifecycles {
                                break;
                            }
                            if let Err(e) = run_true_container(node, bundle, &format!("s{i:06}")) {
                                failed.store(true, Ordering::Relaxed);
                                return (ends, Some(e));
                            }
                            ends.push(Instant::now());
                        }
                        (ends, None)
                    })
                })
                .collect();
            start.wait();
            let (c0, t0) = (cpu_usec(own), Instant::now());
            let (mut ends, mut errs) = (Vec::with_capacity(lifecycles), Vec::new());
            for t in threads {
                match t.join() {
                    Ok((done, err)) => {
                        ends.extend(done);
                        errs.extend(err);
                    }
                    Err(_) => errs.push("a thread panicked".to_owned()),
                }
            }
            let wall = t0.elapsed();
            (t0, wall, (cpu_usec(own) - c0) as f64 / 1e6, ends, errs)
        });
        if let Some(e) = errs.into_iter().next() {
            return Err(format!(
                "{e} (after {} of {lifecycles} lifecycles)",
                ends.len()
            ));
        }
        let mut at: Vec<f64> = ends
            .iter()
            .map(|end| end.saturating_duration_since(t0).as_secs_f64())
            .collect();
        at.sort_by(f64::total_cmp);
        let (mut windows, mut prev) = (Vec::new(), 0.0);
        for k in (SUSTAINED_WINDOW - 1..at.len()).step_by(SUSTAINED_WINDOW) {
            windows.push(SUSTAINED_WINDOW as f64 / (at[k] - prev));
            prev = at[k];
        }
        let last_over_first = (windows.len() > 1).then(|| windows[windows.len() - 1] / windows[0]);
        Ok(
            json!({"concurrency": SUSTAINED_CONCURRENCY, "lifecycles": lifecycles,
                  "wall_s": wall.as_secs_f64(), "per_s": lifecycles as f64 / wall.as_secs_f64(),
                  "cpu_s_per_lifecycle": cpu_s / lifecycles as f64, "window": SUSTAINED_WINDOW,
                  "window_per_s": windows, "last_over_first": last_over_first}),
        )
    }

    /// `lifecycles` of the throughput lifecycle at concurrency `conc`, each thread taking the next until
    /// none are left or one has failed, the clock started once every thread is at the barrier: its wall
    /// time, and the CPU the subtree `own` (the agent and its boxes) used in it, in seconds.
    fn at_concurrency(
        node: &dyn Runtime,
        bundle: &Path,
        own: &Path,
        conc: usize,
        lifecycles: usize,
        tag: &str,
    ) -> Result<(Duration, f64, Vec<f64>), String> {
        let (next, failed) = (AtomicUsize::new(0), AtomicBool::new(false));
        let start = Barrier::new(conc + 1);
        std::thread::scope(|s| {
            let threads: Vec<_> = (0..conc)
                .map(|_| {
                    s.spawn(|| {
                        let mut took = Vec::new();
                        start.wait();
                        while !failed.load(Ordering::Relaxed) {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            if i >= lifecycles {
                                break;
                            }
                            let began = Instant::now();
                            if let Err(e) =
                                run_true_container(node, bundle, &format!("{tag}{i:06}"))
                            {
                                failed.store(true, Ordering::Relaxed);
                                return Err(e);
                            }
                            took.push(began.elapsed().as_secs_f64() * 1e3);
                        }
                        Ok(took)
                    })
                })
                .collect();
            start.wait();
            let (c0, t0) = (cpu_usec(own), Instant::now());
            let (mut took, mut errs) = (Vec::with_capacity(lifecycles), Vec::new());
            for t in threads {
                match t
                    .join()
                    .unwrap_or_else(|_| Err("a thread panicked".to_owned()))
                {
                    Ok(each) => took.extend(each),
                    Err(e) => errs.push(e),
                }
            }
            let (wall, cpu_s) = (t0.elapsed(), (cpu_usec(own) - c0) as f64 / 1e6);
            took.sort_by(f64::total_cmp);
            errs.into_iter().next().map_or(Ok((wall, cpu_s, took)), Err)
        })
    }

    /// `lifecycles` of the throughput lifecycle at concurrency `conc`, as [`at_concurrency`] runs them, with each
    /// one's create and delete timed alone, as the throughput phase times them: the creates' ms and the deletes'.
    fn burst(
        node: &dyn Runtime,
        bundle: &Path,
        conc: usize,
        lifecycles: usize,
        tag: &str,
    ) -> Result<(Vec<f64>, Vec<f64>), String> {
        let (next, failed) = (AtomicUsize::new(0), AtomicBool::new(false));
        let start = Barrier::new(conc + 1);
        std::thread::scope(|s| {
            let threads: Vec<_> = (0..conc)
                .map(|_| {
                    s.spawn(|| {
                        let (mut creates, mut deletes) = (Vec::new(), Vec::new());
                        start.wait();
                        while !failed.load(Ordering::Relaxed) {
                            let i = next.fetch_add(1, Ordering::Relaxed);
                            if i >= lifecycles {
                                break;
                            }
                            match run_true_container_timed(node, bundle, &format!("{tag}{i:03}")) {
                                Ok((create, delete)) => {
                                    creates.push(create.as_secs_f64() * 1e3);
                                    deletes.push(delete.as_secs_f64() * 1e3);
                                }
                                Err(e) => {
                                    failed.store(true, Ordering::Relaxed);
                                    return Err(e);
                                }
                            }
                        }
                        Ok((creates, deletes))
                    })
                })
                .collect();
            start.wait();
            let (mut creates, mut deletes, mut errs) = (
                Vec::with_capacity(lifecycles),
                Vec::with_capacity(lifecycles),
                Vec::new(),
            );
            for t in threads {
                match t
                    .join()
                    .unwrap_or_else(|_| Err("a thread panicked".to_owned()))
                {
                    Ok((c, d)) => {
                        creates.extend(c);
                        deletes.extend(d);
                    }
                    Err(e) => errs.push(e),
                }
            }
            errs.into_iter().next().map_or(Ok((creates, deletes)), Err)
        })
    }

    /// The `serve` phase (goal 17's record, bench/competitive/probes/rounds.py): bursts on request, so a coordinator
    /// can interleave this node with other runtimes round by round, each burst meeting the host as it is then (Ada's
    /// rule: a p99 claim stands on at least 299 interleaved rounds, the load recorded each round). A request is one
    /// line on stdin, `burst <lifecycles> <concurrency>`. Its reply is one JSON line on stdout: each lifecycle's create
    /// and delete in ms, `{"create_ms":[..],"delete_ms":[..]}`, or `{"error":".."}`, after which the phase ends with
    /// that error. Stdin's end ends it, with the bursts and lifecycles it served. A burst's boxes are named by its
    /// number, so no box id comes twice.
    fn serve(node: &dyn Runtime, bundle: &Path) -> Result<Value, String> {
        let (mut bursts, mut lifecycles) = (0usize, 0usize);
        let reply = |v: &Value| {
            let mut out = std::io::stdout().lock();
            writeln!(out, "{v}")
                .and_then(|()| out.flush())
                .map_err(|e| format!("replying: {e}"))
        };
        for line in std::io::stdin().lock().lines() {
            let line = line.map_err(|e| format!("reading a request: {e}"))?;
            let asked = match line.split_whitespace().collect::<Vec<_>>().as_slice() {
                ["burst", n, conc] => n.parse::<usize>().ok().zip(conc.parse::<usize>().ok()),
                _ => None,
            };
            let Some((n, conc)) = asked.filter(|&(n, conc)| n > 0 && conc > 0) else {
                let e = format!(
                    "a request {line:?} isn't `burst <lifecycles> <concurrency>`, each above 0"
                );
                reply(&json!({"error": e}))?;
                return Err(e);
            };
            match burst(node, bundle, conc, n, &format!("v{bursts:06}-")) {
                Ok((create_ms, delete_ms)) => {
                    reply(&json!({"create_ms": create_ms, "delete_ms": delete_ms}))?
                }
                Err(e) => {
                    reply(&json!({"error": e}))?;
                    return Err(e);
                }
            }
            bursts += 1;
            lifecycles += n;
        }
        Ok(json!({"bursts": bursts, "lifecycles": lifecycles}))
    }

    /// The `per_mille` quantile of `sorted`, ascending, by its nearest rank; `None` for none.
    fn quantile(sorted: &[f64], per_mille: usize) -> Option<f64> {
        let n = sorted.len();
        (n > 0).then(|| sorted[(n * per_mille).div_ceil(1000).clamp(1, n) - 1])
    }

    /// The host's 1-minute load average and its CPU pressure, the share of the last 10 s some task waited
    /// for a CPU (`/proc/pressure/cpu`'s `some avg10`), as the breakdown probes note them beside each round.
    fn host_load() -> Value {
        let load1 = std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|l| l.split_whitespace().next()?.parse::<f64>().ok());
        let cpu_some_avg10 = std::fs::read_to_string("/proc/pressure/cpu")
            .ok()
            .and_then(|p| {
                p.lines()
                    .next()?
                    .split_whitespace()
                    .find_map(|w| w.strip_prefix("avg10="))?
                    .parse::<f64>()
                    .ok()
            });
        json!({"load1": load1, "cpu_some_avg10": cpu_some_avg10})
    }

    /// Concurrency 1 and [`SUSTAINED_CONCURRENCY`] in turn, until the ratio is resolved against the bar
    /// ([`INTERLEAVED_FEWEST`] to [`INTERLEAVED_MOST`] pairs): [`INTERLEAVED_C1`]
    /// lifecycles at 1, then [`INTERLEAVED_C32`] at 32, as the sustained phase runs them, so both meet the host as it
    /// is then. Each pair's rates and their ratio, with the host's load and CPU pressure at its start and end
    /// and each half's CPU, per lifecycle and as the CPUs it kept busy on average (the agent and its boxes,
    /// `own`'s cpu.stat): whether concurrency 32 is held by the CPUs it has or by waiting. And the ratio of
    /// the pooled rates, each concurrency's lifecycles over its time, and why the round stopped (`resolved`
    /// against the bar, or `unresolved` at the most pairs).
    fn interleaved(node: &dyn Runtime, bundle: &Path, own: &Path) -> Result<Value, String> {
        let (cpus1, cpus32) = (
            cpu_list("HEXBENCH_INTERLEAVED_CPUS1"),
            cpu_list("HEXBENCH_INTERLEAVED_CPUS32"),
        );
        let started_on = rustix::thread::sched_getaffinity(None)
            .map_err(|e| format!("reading this process's CPUs: {e}"))?;
        let pin_to = |cpus: &Option<(String, rustix::thread::CpuSet)>| {
            cpus.as_ref().map_or(Ok(Value::Null), |(_, set)| pin(set))
        };
        let (mut rows, mut ratios) = (Vec::new(), Vec::new());
        let (mut one_s, mut many_s) = (0.0, 0.0);
        let mut stopped = "unresolved";
        for p in 0..INTERLEAVED_MOST {
            if p >= INTERLEAVED_FEWEST && resolved(&ratios) {
                stopped = "resolved";
                break;
            }
            let at_start = host_load();
            let c1_pinned = pin_to(&cpus1)?;
            let (one, one_cpu, one_took) =
                at_concurrency(node, bundle, own, 1, INTERLEAVED_C1, &format!("i{p:03}a"))?;
            let c32_pinned = pin_to(&cpus32)?;
            let (many, many_cpu, many_took) = at_concurrency(
                node,
                bundle,
                own,
                SUSTAINED_CONCURRENCY,
                INTERLEAVED_C32,
                &format!("i{p:03}b"),
            )?;
            let (r1, r32) = (
                INTERLEAVED_C1 as f64 / one.as_secs_f64(),
                INTERLEAVED_C32 as f64 / many.as_secs_f64(),
            );
            one_s += one.as_secs_f64();
            many_s += many.as_secs_f64();
            ratios.push(r32 / r1);
            rows.push(json!({"c1_per_s": r1, "c32_per_s": r32, "ratio": r32 / r1,
                             "c1_cpu_ms_per_lifecycle": one_cpu * 1e3 / INTERLEAVED_C1 as f64,
                             "c32_cpu_ms_per_lifecycle": many_cpu * 1e3 / INTERLEAVED_C32 as f64,
                             "c1_cpus_busy": one_cpu / one.as_secs_f64(),
                             "c32_cpus_busy": many_cpu / many.as_secs_f64(),
                             "c1_p50_ms": quantile(&one_took, 500),
                             "c1_p99_ms": quantile(&one_took, 990),
                             "c32_p50_ms": quantile(&many_took, 500),
                             "c32_p99_ms": quantile(&many_took, 990),
                             "c1_pinned": c1_pinned, "c32_pinned": c32_pinned,
                             "at_start": at_start, "at_end": host_load()}));
        }
        if cpus1.is_some() || cpus32.is_some() {
            pin(&started_on)?;
        }
        if resolved(&ratios) {
            stopped = "resolved";
        }
        ratios.sort_by(f64::total_cmp);
        let ran = rows.len();
        let c1 = (ran * INTERLEAVED_C1) as f64 / one_s;
        let c32 = (ran * INTERLEAVED_C32) as f64 / many_s;
        Ok(
            json!({"pairs": rows, "stopped": stopped, "bar": INTERLEAVED_BAR,
                  "concurrency": SUSTAINED_CONCURRENCY, "c1_lifecycles": INTERLEAVED_C1,
                  "c32_lifecycles": INTERLEAVED_C32, "c1_per_s": c1, "c32_per_s": c32, "ratio": c32 / c1,
                  "ratio_median": ratios.get(ratios.len() / 2), "ratio_min": ratios.first(),
                  "ratio_max": ratios.last(), "c1_cpus": cpus1.map(|(l, _)| l),
                  "c32_cpus": cpus32.map(|(l, _)| l)}),
        )
    }

    /// The most tasks a pin walks: the kernel's own bound on the tasks that can exist at once,
    /// `/proc/sys/kernel/threads-max` (Documentation/admin-guide/sysctl/kernel.rst, "threads-max"), read on the
    /// host the bench runs on, so the walk ends however the tree it pins is shaped.
    fn pin_tasks() -> Result<usize, String> {
        let raw = std::fs::read_to_string("/proc/sys/kernel/threads-max")
            .map_err(|e| format!("reading /proc/sys/kernel/threads-max: {e}"))?;
        raw.trim()
            .parse()
            .map_err(|e| format!("/proc/sys/kernel/threads-max {raw:?}: {e}"))
    }

    /// The CPU list in `var` (`4-15`, `4-15,20`) and its set; `None` when it's unset or empty.
    fn cpu_list(var: &str) -> Option<(String, rustix::thread::CpuSet)> {
        let list = std::env::var(var).ok().filter(|v| !v.is_empty())?;
        let mut set = rustix::thread::CpuSet::new();
        for part in list.split(',') {
            let (lo, hi) = part.split_once('-').unwrap_or((part, part));
            let cpu = |n: &str| -> usize {
                let c = n
                    .trim()
                    .parse()
                    .unwrap_or_else(|e| panic!("{var}={list:?}: {e}"));
                assert!(
                    c < rustix::thread::CpuSet::MAX_CPU,
                    "{var}={list:?}: CPU {c}"
                );
                c
            };
            for c in cpu(lo)..=cpu(hi) {
                set.set(c);
            }
        }
        Some((list, set))
    }

    /// Pins every thread of this process and of its descendants (the socket driver's node agent, the agent's
    /// create seeds, the boxes' inits) to `cpus`, so what they start after inherits it, walking the tree
    /// through each task's `children`; a task that ends meanwhile is passed over. Then reads each pinned task's
    /// `Cpus_allowed_list` back: how many were pinned, and how many hold each list, which should be `cpus`'
    /// alone.
    fn pin(cpus: &rustix::thread::CpuSet) -> Result<Value, String> {
        let most = pin_tasks()?;
        let mut procs = vec![std::process::id()];
        let mut tasks = Vec::new();
        let mut next = 0;
        while let Some(&pid) = procs.get(next) {
            next += 1;
            let Ok(dir) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
                continue;
            };
            for task in dir.flatten() {
                let Some(tid) = task
                    .file_name()
                    .to_str()
                    .and_then(|t| t.parse::<i32>().ok())
                else {
                    continue;
                };
                if tasks.len() >= most {
                    return Err(format!(
                        "more tasks to pin than threads-max's {most}: the walk met tasks again"
                    ));
                }
                match rustix::thread::sched_setaffinity(rustix::process::Pid::from_raw(tid), cpus) {
                    Ok(()) => tasks.push(tid),
                    Err(rustix::io::Errno::SRCH) => continue,
                    Err(e) => return Err(format!("pinning task {tid}: {e}")),
                }
                if let Ok(children) =
                    std::fs::read_to_string(format!("/proc/{pid}/task/{tid}/children"))
                {
                    procs.extend(
                        children
                            .split_whitespace()
                            .filter_map(|c| c.parse::<u32>().ok()),
                    );
                }
            }
        }
        let mut allowed: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        for tid in &tasks {
            let status = std::fs::read_to_string(format!("/proc/{tid}/status")).unwrap_or_default();
            if let Some(list) = status
                .lines()
                .find_map(|l| l.strip_prefix("Cpus_allowed_list:"))
            {
                *allowed.entry(list.trim().to_owned()).or_default() += 1;
            }
        }
        Ok(json!({"tasks": tasks.len(), "cpus_allowed": allowed}))
    }

    /// n sequential create+start of a `true` container after `warmup` untimed ones, the two calls'
    /// times summed and the box deleted untimed, as oci_bench.py's `start_true` times libcrun's
    /// (AC-T7.1.3's measure): delete runs whether or not start did, so no box is left behind.
    fn start_true(
        node: &dyn Runtime,
        bundle: &Path,
        n: usize,
        warmup: usize,
    ) -> Result<Value, String> {
        let mut ms = Vec::with_capacity(n);
        for i in 0..warmup + n {
            let id = format!("t{i:06}");
            let bid = BoxId::new(id.as_str()).map_err(|e| e.to_string())?;
            let create = Instant::now();
            node.create(spec(&bid, bundle))
                .map_err(|e| format!("create {id}: {e}"))?;
            let create = create.elapsed();
            let start = Instant::now();
            let started = node.start(&bid).map_err(|e| format!("start {id}: {e}"));
            let start = start.elapsed();
            node.delete(&bid).map_err(|e| format!("delete {id}: {e}"))?;
            started?;
            if i >= warmup {
                ms.push((create + start).as_secs_f64() * 1e3);
            }
        }
        Ok(json!({"ms": ms}))
    }

    /// create+start+delete of a `true` container, with no exec: the throughput lifecycle.
    fn run_true_container(node: &dyn Runtime, bundle: &Path, id: &str) -> Result<(), String> {
        run_true_container_timed(node, bundle, id).map(drop)
    }

    /// The complete create is timed alongside part 19, under the same concurrency and host load, and so is the
    /// delete, which pays for what a park does (decision 8).
    fn run_true_container_timed(
        node: &dyn Runtime,
        bundle: &Path,
        id: &str,
    ) -> Result<(Duration, Duration), String> {
        let bid = BoxId::new(id).map_err(|e| e.to_string())?;
        let spec = spec(&bid, bundle);
        let t = Instant::now();
        node.create(spec).map_err(|e| format!("create {id}: {e}"))?;
        let create = t.elapsed();
        let started = node.start(&bid).map_err(|e| format!("start {id}: {e}"));
        let t = Instant::now();
        node.delete(&bid).map_err(|e| format!("delete {id}: {e}"))?;
        let delete = t.elapsed();
        started.map(|_| (create, delete))
    }

    /// Inspect of one running `sleep` container, `n` times after `warmup`, each timed alone: a call
    /// whose work (the box's record and its container's state, read) is the same through either
    /// driver, so the drivers' difference is the socket's cost per call. Then, untimed, `n` usage reads and `n`
    /// SIGCONTs (which a running `sleep` ignores), so the waits census's `client.answer.usage` and `.signal` rows have
    /// their answers (bench/competitive/probes/waits_census.md).
    fn rtt(node: &dyn Runtime, bundle: &Path, n: usize, warmup: usize) -> Result<Value, String> {
        let id = BoxId::new("rtt").map_err(|e| e.to_string())?;
        node.create(spec(&id, bundle))
            .map_err(|e| format!("create rtt: {e}"))?;
        let timed = (|| -> Result<Vec<f64>, String> {
            node.start(&id).map_err(|e| format!("start rtt: {e}"))?;
            let mut ms = Vec::with_capacity(n);
            for i in 0..warmup + n {
                let t = Instant::now();
                node.inspect(&id).map_err(|e| format!("inspect rtt: {e}"))?;
                if i >= warmup {
                    ms.push(t.elapsed().as_secs_f64() * 1e3);
                }
            }
            let cont = Signal::new(18).map_err(|e| e.to_string())?;
            for _ in 0..n {
                node.usage(&id).map_err(|e| format!("usage rtt: {e}"))?;
                node.signal(&id, cont)
                    .map_err(|e| format!("signal rtt: {e}"))?;
            }
            Ok(ms)
        })();
        node.delete(&id).map_err(|e| format!("delete rtt: {e}"))?;
        Ok(json!({ "ms": timed? }))
    }

    /// The waits census's list arm (bench/competitive/probes/waits_census.md, `serve.reads`): `boxes` boxes made, then
    /// `lists` lists of them, each timed, so a list's parts wait for their reader over the socket; the boxes deleted
    /// after. `HEXBENCH_LIST_BOXES` and `HEXBENCH_LISTS` set them (the phase's N and 100 unset).
    fn list(node: &dyn Runtime, bundle: &Path, n: usize) -> Result<Value, String> {
        let count = |var: &str, unset: usize| {
            std::env::var(var).map_or(unset, |v| {
                v.trim()
                    .parse()
                    .unwrap_or_else(|e| panic!("{var}={v:?}: {e}"))
            })
        };
        let (boxes, lists) = (
            count("HEXBENCH_LIST_BOXES", n),
            count("HEXBENCH_LISTS", 100),
        );
        let ids: Vec<BoxId> = (0..boxes)
            .map(|i| BoxId::new(&format!("list{i}")).map_err(|e| e.to_string()))
            .collect::<Result<_, _>>()?;
        let mut made = 0;
        let timed = (|| -> Result<Vec<f64>, String> {
            for id in &ids {
                node.create(spec(id, bundle))
                    .map_err(|e| format!("create {id}: {e}"))?;
                made += 1;
            }
            let mut ms = Vec::with_capacity(lists);
            for _ in 0..lists {
                let t = Instant::now();
                let listed = node.list().map_err(|e| format!("list: {e}"))?;
                ms.push(t.elapsed().as_secs_f64() * 1e3);
                if listed.len() < boxes {
                    return Err(format!("a list of {boxes} boxes gave {}", listed.len()));
                }
            }
            Ok(ms)
        })();
        for id in ids.iter().take(made) {
            node.delete(id).map_err(|e| format!("delete {id}: {e}"))?;
        }
        Ok(json!({ "boxes": boxes, "ms": timed? }))
    }

    /// The waits census's restart arm (`node.listing`): over the socket driver, `boxes` boxes made, the agent ended
    /// with SIGTERM and started again over the same state and box root, so its recovery lists their parts; then the
    /// boxes, adopted, deleted. `HEXBENCH_RESTART_BOXES` sets `boxes` (the phase's N unset). Returns the driver
    /// with the new agent, and how long the new agent took to say it was ready.
    fn restart(
        driver: Driver,
        hex: &str,
        state: &Path,
        root: &Path,
        bundle: &Path,
        n: usize,
    ) -> (Driver, Result<Value, String>) {
        let Driver::Socket { client, agent } = driver else {
            return (
                driver,
                Err("the restart arm needs HEXBENCH_DRIVER=socket".to_owned()),
            );
        };
        let boxes = std::env::var("HEXBENCH_RESTART_BOXES").map_or(n, |v| {
            v.trim()
                .parse()
                .unwrap_or_else(|e| panic!("HEXBENCH_RESTART_BOXES={v:?}: {e}"))
        });
        let ids: Vec<BoxId> = (0..boxes)
            .map(|i| BoxId::new(&format!("restart{i}")).expect("a box id"))
            .collect();
        let mut made = 0;
        let created = ids.iter().try_for_each(|id| {
            client
                .create(spec(id, bundle))
                .map(|_| made += 1)
                .map_err(|e| format!("create {id}: {e}"))
        });
        drop(client);
        let ended = agent.end();
        let t = Instant::now();
        let (agent, path) = Agent::start(hex, state, root);
        let ready_ms = t.elapsed().as_secs_f64() * 1e3;
        let client = Client::connect(&path)
            .unwrap_or_else(|e| panic!("connecting to hex node at {}: {e}", path.display()));
        let deleted = ids
            .iter()
            .take(made)
            .try_for_each(|id| client.delete(id).map_err(|e| format!("delete {id}: {e}")));
        let result = created
            .and(ended.map(|_| ()))
            .and(deleted)
            .map(|()| json!({ "boxes": boxes, "ready_ms": ready_ms }));
        (Driver::Socket { client, agent }, result)
    }

    /// How long a socket agent has to end on SIGTERM before it's SIGKILLed: its end is one more exchange with the
    /// agent (a counts line and an exit), so it has the bound each call has, [`CALL_TIMEOUT`], not one of its own.
    const AGENT_END: Duration = CALL_TIMEOUT;

    /// A `hex node` serving its socket for the socket driver, its boxes under `box_root`; ended with SIGTERM
    /// ([`Agent::end`]), else killed when dropped. It runs as a node ships, its pools and ways its own: what this
    /// bench sets, it sets on the in-process node alone, and the agent raises its soft descriptor limit to the hard one as it starts, undoing HEXBENCH_NOFILE's `ulimit -Sn`; so an A/B of any of
    /// those reads the in-process figures, never the socket's. Its stderr is passed on as it comes and kept.
    struct Agent {
        child: Child,
        said: std::sync::Arc<std::sync::Mutex<String>>,
        drain: Option<std::thread::JoinHandle<()>>,
    }

    impl Agent {
        fn start(hex: &str, state: &Path, box_root: &Path) -> (Self, PathBuf) {
            let socket = state.join("node.sock");
            let mut cmd = Command::new(hex);
            cmd.arg("node")
                .arg("--socket")
                .arg(&socket)
                .arg("--state")
                .arg(state.join("node"))
                .arg("--box-root")
                .arg(box_root)
                .stdout(Std::piped())
                .stderr(Std::piped());
            let mut child = cmd.spawn().expect("start hex node");
            let said = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
            let drain = {
                let said = std::sync::Arc::clone(&said);
                let err = child.stderr.take().expect("hex node's stderr");
                std::thread::spawn(move || {
                    for line in BufReader::new(err).lines().map_while(Result::ok) {
                        eprintln!("{line}");
                        let mut said = said.lock().expect("hex node's stderr kept");
                        said.push_str(&line);
                        said.push('\n');
                    }
                })
            };
            let mut line = String::new();
            BufReader::new(child.stdout.take().expect("hex node's stdout"))
                .read_line(&mut line)
                .expect("hex node's ready line");
            assert!(line.starts_with("ready "), "hex node said {line:?}");
            (
                Self {
                    child,
                    said,
                    drain: Some(drain),
                },
                socket,
            )
        }

        /// Ends the node with SIGTERM, waits at most [`AGENT_END`] for it (SIGKILLed past it) on its pidfd, which
        /// reads ready the moment it exits, and returns the parked procs' counts it said as it ended, each by name.
        fn end(mut self) -> Result<Value, String> {
            let pid = rustix::process::Pid::from_child(&self.child);
            let exited = rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty())
                .map_err(|e| format!("hex node's pidfd: {e}"))?;
            rustix::process::kill_process(pid, rustix::process::Signal::TERM)
                .map_err(|e| format!("SIGTERM to hex node: {e}"))?;
            let deadline = Instant::now() + AGENT_END;
            let ended = loop {
                let left = deadline.saturating_duration_since(Instant::now());
                let left = rustix::event::Timespec {
                    tv_sec: i64::try_from(left.as_secs()).unwrap_or(i64::MAX),
                    tv_nsec: i64::from(left.subsec_nanos()),
                };
                let mut fds = [rustix::event::PollFd::new(
                    &exited,
                    rustix::event::PollFlags::IN,
                )];
                match rustix::event::poll(&mut fds, Some(&left)) {
                    Ok(0) => break None,
                    Ok(_) => {
                        break Some(
                            self.child
                                .wait()
                                .map_err(|e| format!("reaping hex node: {e}"))?,
                        );
                    }
                    Err(rustix::io::Errno::INTR) => continue,
                    Err(e) => return Err(format!("waiting on hex node's pidfd: {e}")),
                }
            };
            let Some(status) = ended else {
                let _ = self.child.kill();
                let _ = self.child.wait();
                return Err(format!(
                    "hex node didn't end in {AGENT_END:?} of SIGTERM: SIGKILLed"
                ));
            };
            if let Some(drain) = self.drain.take() {
                let _ = drain.join();
            }
            let said = self.said.lock().expect("hex node's stderr kept").clone();
            let line = said
                .lines()
                .find_map(|l| l.strip_prefix("hex node: parked procs at exit: "))
                .ok_or_else(|| format!("hex node ended {status} saying no parked procs line"))?;
            let mut counts = Map::new();
            for kv in line.split(", ") {
                let (k, v) = kv
                    .split_once(' ')
                    .ok_or_else(|| format!("hex node's parked procs line {line:?}"))?;
                let v: u64 = v
                    .parse()
                    .map_err(|e| format!("hex node's parked procs line {line:?}: {e}"))?;
                counts.insert(k.to_owned(), json!(v));
            }
            Ok(Value::Object(counts))
        }
    }

    impl Drop for Agent {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    /// The seam, by one of its drivers.
    enum Driver {
        /// The node agent in this process.
        InProcess(Box<Node>),
        /// A `hex node` of its own, reached through its socket. The client is dropped before the agent
        /// it holds a connection to, which is kept for its drop alone.
        Socket { client: Client, agent: Agent },
    }

    impl Driver {
        fn runtime(&self) -> &dyn Runtime {
            match self {
                Self::InProcess(node) => node.as_ref(),
                Self::Socket { client, .. } => client,
            }
        }

        /// The node's create seeds' counts, in process; the socket carries none.
        fn seeds(&self) -> Result<Option<seeds::Counts>, String> {
            match self {
                Self::InProcess(node) => node
                    .create_seeds()
                    .map(Some)
                    .map_err(|e| format!("the node's create seeds' counts: {e}")),
                Self::Socket { .. } => Ok(None),
            }
        }

        /// What the node's idle cgroups and its deletes have done, in process; the socket carries
        /// neither.
        fn reuse(&self) -> Option<(hex_node::cgroups::Counts, hex_node::cgroups::Kills)> {
            match self {
                Self::InProcess(node) => node.cgroup_reuse().ok().map(|c| (c, node.kills())),
                Self::Socket { .. } => None,
            }
        }

        /// What the network namespaces the node keeps for owners have done, in process; the socket
        /// carries none.
        fn netns_reuse(&self) -> Option<hex_node::netns::ReuseCounts> {
            match self {
                Self::InProcess(node) => node.netns_reuse().ok().flatten(),
                Self::Socket { .. } => None,
            }
        }

        /// The creates its node made with the classic root as goal 17's baseline arm's
        /// (`Node::with_classic_roots`, counted under `ClassicReason::Baseline`), and those a pruned seed made, in
        /// process only, as `/dev`'s counts are.
        fn roots(&self) -> Option<[u64; 2]> {
            match self {
                Self::InProcess(node) => Some([
                    node.classic_creates_for(hex_runtime::linux::oci::run::ClassicReason::Baseline),
                    node.pruned_creates(),
                ]),
                Self::Socket { .. } => None,
            }
        }

        /// Direct preparation counters; the socket wire remains unchanged.
        fn dev_mounts(&self) -> Result<Option<hex_core::devfs::Counts>, String> {
            match self {
                Self::InProcess(node) => node.dev_mounts().map(Some).map_err(|e| e.to_string()),
                Self::Socket { .. } => Ok(None),
            }
        }

        /// The node's ipc and uts namespace counts, read in process, as its create seeds' are.
        fn ipc_uts(&self) -> Result<Option<netns::Counts>, String> {
            match self {
                Self::InProcess(node) => node.ipc_uts().map(Some).map_err(|e| e.to_string()),
                Self::Socket { .. } => Ok(None),
            }
        }

        /// The prepared proc pool's counters and the creates by how their mount namespace was made
        /// (decision 6's update), in process only, as `/dev`'s.
        fn proc_mounts(&self) -> Result<Option<(hex_core::devfs::Counts, [u64; 3])>, String> {
            use hex_runtime::linux::oci::mntns::MountNamespace as W;
            match self {
                Self::InProcess(node) => node
                    .proc_mounts()
                    .map(|c| {
                        Some((
                            c,
                            [W::Copy, W::Empty, W::FromTree].map(|w| node.mount_namespaces_made(w)),
                        ))
                    })
                    .map_err(|e| e.to_string()),
                Self::Socket { .. } => Ok(None),
            }
        }

        /// The node's network namespace counts: a node method in process, a call over the socket.
        fn netns(&self) -> Result<netns::Counts, String> {
            match self {
                Self::InProcess(node) => node.netns(),
                Self::Socket { client, .. } => client.netns(),
            }
            .map_err(|e| format!("the node's network namespace counts: {e}"))
        }
    }

    /// What the node's network namespace counts rose by from `before` to `after`.
    fn rise(before: netns::Counts, after: netns::Counts) -> Value {
        json!({"made_ahead": after.made_ahead - before.made_ahead,
               "taken": after.taken - before.taken,
               "made_on_path": after.made_on_path - before.made_on_path,
               "discarded": after.discarded - before.discarded,
               "failed": after.failed - before.failed})
    }

    /// What the node's create seeds' counts rose by from `before` to `after`, and the most busy at once
    /// so far (a high-water mark, not a rise).
    fn seeds_rise(before: seeds::Counts, after: seeds::Counts) -> Value {
        json!({"asked": after.asked - before.asked,
               "waited": after.waited - before.waited,
               "refused": (after.refused_busy + after.refused_waited + after.refused_died + after.refused_hung)
                   - (before.refused_busy + before.refused_waited + before.refused_died + before.refused_hung),
               "started": after.started - before.started,
               "restarted": after.restarted - before.restarted,
               "start_failed": after.start_failed - before.start_failed,
               "died": after.died - before.died,
               "hung": after.hung - before.hung,
               "most_busy": after.most_busy})
    }

    /// What each idle cgroup an owner's boxes left still has charged (`memory.current`, bytes), read at a
    /// phase's end, when no box is live, so every slot under the oci boxes' root (`.<n>`) is one the
    /// node's check would read before handing it on. `hex_node::cgroups::RESIDUAL` is set from these
    /// (bench/competitive/probes/reuse_residual.md). A slot the node removes while it's read is skipped.
    fn idle_residuals(oci_boxes: &Path) -> Vec<u64> {
        let Ok(entries) = std::fs::read_dir(oci_boxes) else {
            return Vec::new();
        };
        let mut out: Vec<u64> = entries
            .filter_map(Result::ok)
            .filter(|e| {
                e.file_name().to_str().is_some_and(|n| {
                    n.strip_prefix('.')
                        .is_some_and(|d| !d.is_empty() && d.bytes().all(|b| b.is_ascii_digit()))
                })
            })
            .filter_map(|e| std::fs::read_to_string(e.path().join("memory.current")).ok())
            .filter_map(|t| t.trim().parse().ok())
            .collect();
        out.sort_unstable();
        out
    }

    /// What the network namespaces kept for owners did from `before` to `after`.
    fn netns_reuse_rise(
        before: &hex_node::netns::ReuseCounts,
        after: &hex_node::netns::ReuseCounts,
    ) -> Value {
        let (b, a) = (&before.pool, &after.pool);
        let refused: Map<String, Value> = hex_core::netns_reuse::Refused::ALL
            .iter()
            .filter_map(|r| {
                let n = a.refused(*r) - b.refused(*r);
                (n > 0).then(|| (r.name().to_owned(), json!(n)))
            })
            .collect();
        json!({"handed_on": a.handed_on - b.handed_on,
               "fresh": a.fresh - b.fresh,
               "refused": refused,
               "evicted": (a.evicted_owner_bound + a.evicted_node_bound)
                   - (b.evicted_owner_bound + b.evicted_node_bound),
               "idle": a.idle,
               "given_back": after.given_back - before.given_back,
               "checked": after.checked - before.checked,
               "unchecked": after.unchecked - before.unchecked,
               "lost_home": after.lost_home - before.lost_home,
               "made_on_path": after.made_on_path - before.made_on_path,
               "protocols": after.protocols - before.protocols,
               "through_files": after.through_files - before.through_files,
               "files_failed": after.files_failed - before.files_failed,
               "files_bounded": after.files_bounded - before.files_bounded,
               "files_held": after.files_held,
               "files_bound": after.files_bound.map(|f| f.namespaces),
               "descriptors_soft": after.files_bound.and_then(|f| f.soft),
               "descriptors_own": after.files_bound.map(|f| f.own),
               "check_us_mean": (after.check_ns - before.check_ns) as f64
                   / 1e3 / ((after.checked - before.checked).max(1)) as f64})
    }

    /// What the node's idle cgroups and its deletes did from `before` to `after`.
    fn reuse_rise(
        before: &(hex_node::cgroups::Counts, hex_node::cgroups::Kills),
        after: &(hex_node::cgroups::Counts, hex_node::cgroups::Kills),
    ) -> Value {
        let (b, a) = (&before.0.pool, &after.0.pool);
        let refused: Map<String, Value> = hex_core::reuse::Refused::ALL
            .iter()
            .filter_map(|r| {
                let n = a.refused(*r) - b.refused(*r);
                (n > 0).then(|| (r.name().to_owned(), json!(n)))
            })
            .collect();
        json!({"handed_on": a.handed_on - b.handed_on,
               "fresh": a.fresh - b.fresh,
               "missed": a.missed - b.missed,
               "refused": refused,
               "evicted": (a.evicted_owner_bound + a.evicted_node_bound + a.evicted_refused)
                   - (b.evicted_owner_bound + b.evicted_node_bound + b.evicted_refused),
               "idle": a.idle,
               "removals_failed": after.0.removals_failed - before.0.removals_failed,
               "ended_through_init": after.1.through_init - before.1.through_init,
               "fell_back_to_cgroup_kill": after.1.fell_back - before.1.fell_back,
               "cgroup_kill": after.1.cgroup_kill - before.1.cgroup_kill,
               "parked": parked_rise(&before.0.parked, &after.0.parked),
               "ahead": {"made": after.0.ahead.made - before.0.ahead.made,
                         "taken": after.0.ahead.taken - before.0.ahead.taken,
                         "dry": after.0.ahead.dry - before.0.ahead.dry,
                         "failed": after.0.ahead.failed - before.0.ahead.failed,
                         "ready": after.0.ahead.ready}})
    }

    /// What the inits made ahead in idle cgroups did over a phase (decision 8, "The p99.9 that stayed").
    fn parked_rise(b: &hex_node::cgroups::ParkCounts, a: &hex_node::cgroups::ParkCounts) -> Value {
        json!({"parked": a.parked - b.parked, "claimed": a.claimed - b.claimed,
               "ended_unclaimed": a.ended_unclaimed - b.ended_unclaimed,
               "failed": a.failed - b.failed,
               "charged_bytes": a.charged_bytes - b.charged_bytes,
               "procs_made": a.procs_made - b.procs_made,
               "procs_made_at_delete": a.procs_made_at_delete - b.procs_made_at_delete,
               "procs_made_at_refill": a.procs_made_at_refill - b.procs_made_at_refill,
               "proc_hits": a.proc_hits - b.proc_hits,
               "proc_misses": a.proc_misses - b.proc_misses,
               "proc_dry": a.proc_dry - b.proc_dry,
               "procs_failed": a.procs_failed - b.procs_failed,
               "procs_held": a.procs_held,
               "procs_queued": a.procs_queued - b.procs_queued,
               "procs_dropped": a.procs_dropped - b.procs_dropped,
               "procs_dropped_inline": a.procs_dropped_inline - b.procs_dropped_inline})
    }

    pub(super) fn main() -> ExitCode {
        // The waits census probe's switch (bench/competitive/probes/waits_census.md): the in-process node's waits and
        // this client's answers, recorded from their own timing points.
        hex_node::census::install_from_env().expect("the waits census installs");
        let args: Vec<String> = std::env::args_os()
            .skip(1)
            .map(|a| {
                a.into_string()
                    .unwrap_or_else(|a| panic!("argument {a:?} isn't Unicode"))
            })
            .filter(|a| a != "--bench")
            .collect();
        let ([out, n, warmup, lifecycles, hex, sleep_bundle, true_bundle]
        | [
            out,
            n,
            warmup,
            lifecycles,
            hex,
            sleep_bundle,
            true_bundle,
            _,
        ]) = args.as_slice()
        else {
            panic!(
                "usage: seam_oci <out.json> <n> <warmup> <lifecycles> <hex-binary> <sleep-bundle> <true-bundle> [<sleep-nonet-bundle>]"
            );
        };
        let nonet_bundle = args.get(7).map(PathBuf::from);
        let (n, warmup, lifecycles): (usize, usize, usize) = (
            n.parse().unwrap(),
            warmup.parse().unwrap(),
            lifecycles.parse().unwrap(),
        );
        let sustained_lifecycles = std::env::var("HEXBENCH_SUSTAINED")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .map_or(SUSTAINED_DEFAULT, |v| {
                v.trim()
                    .parse()
                    .unwrap_or_else(|e| panic!("HEXBENCH_SUSTAINED={v:?}: {e}"))
            });
        let (sleep_bundle, true_bundle) = (PathBuf::from(sleep_bundle), PathBuf::from(true_bundle));
        // HEXBENCH_REUSE=0: the owner opts out of reuse, its boxes and bundles otherwise as with reuse on, so
        // a pass with it off differs from one with it on in that alone.
        let reuse = std::env::var("HEXBENCH_REUSE").map_or(true, |v| v.trim() != "0");
        let owner = std::env::var("HEXBENCH_OWNER")
            .ok()
            .filter(|o| !o.trim().is_empty())
            .map(|o| {
                BoxOwner::named(o.trim()).unwrap_or_else(|e| panic!("HEXBENCH_OWNER={o:?}: {e}"))
            })
            .map(|o| BoxOwner { reuse, ..o });
        OWNER.set(owner.clone()).expect("the owner is set once");
        // The box root beside the runtimes' container cgroups, under the vacated, delegated cgroup
        // root; the node agent itself stays where it is, and the parent's cpu.stat is the CPU a
        // throughput level used (it covers the agent and every container).
        let parent = box_root_parent();
        let cache =
            PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/hex-t7/ociseam");
        std::fs::create_dir_all(&cache).expect("the state's cache dir");
        let state = tempfile::Builder::new()
            .prefix("ociseam-")
            .tempdir_in(&cache)
            .expect("a state dir");
        let socket = match std::env::var("HEXBENCH_DRIVER").as_deref() {
            Ok("socket") => true,
            Ok("inprocess") | Err(_) => false,
            Ok(other) => panic!("HEXBENCH_DRIVER={other:?}: inprocess or socket"),
        };
        // HEXBENCH_ROOT=classic: goal 17's baseline arm (rounds.py's hex:classic; the lead's ruling of 2026-11-09):
        // a node that builds every root the classic way, by hex-node's test control, never a production knob. Only
        // the node in this process takes it: a socket's hex node takes no test control.
        let classic = match std::env::var("HEXBENCH_ROOT").as_deref() {
            Ok("classic") if socket => {
                panic!("HEXBENCH_ROOT=classic: the classic arm's node is the in-process one")
            }
            Ok("classic") => true,
            Ok(other) => panic!("HEXBENCH_ROOT={other:?}: the one root an arm asks for is classic"),
            Err(_) => false,
        };
        let (runtime, root) = if socket {
            ("hex-seam-socket", parent.join("hex-oci-seam-sock"))
        } else {
            ("hex-seam", parent.join("hex-oci-seam"))
        };
        // The socket driver's agent is a process of its own, as a caller that doesn't link Hex meets
        // it; the in-process driver's is this one.
        let driver = if socket {
            let (agent, path) = Agent::start(hex, state.path(), &root);
            let client = Client::connect(&path)
                .unwrap_or_else(|e| panic!("connecting to hex node at {}: {e}", path.display()));
            Driver::Socket { client, agent }
        } else {
            let node = Node::open(&root, state.path(), state.path())
                .unwrap_or_else(|e| {
                    panic!(
                        "the node agent (box root {}; the oci pass runs this as root in its privileged container): {e}",
                        root.display()
                    )
                });
            // HEX_OCI_MNTNS: the way a pruned create's mount namespace is made, for decision 6's A/B, set on this
            // in-process node's builder alone (the node agent reads no environment for it, Ada's 2026-11-06 rule);
            // unset, the node's default. It stays until the 7.1/7.2 host record decides that A/B, then goes.
            let node = if classic {
                node.with_classic_roots()
                    .unwrap_or_else(|e| panic!("the classic arm's node: {e}"))
            } else {
                node
            };
            let node = match std::env::var("HEX_OCI_MNTNS") {
                Ok(way) => node.with_mount_namespace(&way),
                Err(_) => node,
            }
            .with_oci_helper(PathBuf::from(hex));
            Driver::InProcess(Box::new(node))
        };
        let node = driver.runtime();
        let wanted = std::env::var("HEXBENCH_PHASES")
            .ok()
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| {
                "lifecycle lifecycle_nonet start_true throughput sustained rtt".to_owned()
            });
        let trace = Trace::from_env();
        let (mut phases, mut errors) = (Map::new(), Map::new());
        for phase in [
            "lifecycle",
            "lifecycle_nonet",
            "start_true",
            "throughput",
            "sustained",
            "interleaved",
            "rtt",
            "list",
            "counts",
            "serve",
        ] {
            if !wanted.split_whitespace().any(|w| w == phase) {
                continue;
            }
            let t0 = Instant::now();
            // The node's counts before and after the phase, outside its timing: what it added.
            let r = driver.netns().and_then(|before| {
                let seeds_before = driver.seeds()?;
                let reuse_before = driver.reuse();
                let netns_reuse_before = driver.netns_reuse();
                let dev_before = driver.dev_mounts()?;
                let ipc_uts_before = driver.ipc_uts()?;
                let proc_before = driver.proc_mounts()?;
                let roots_before = driver.roots();
                let mut v = match phase {
                    "lifecycle" => lifecycle(node, &sleep_bundle, n, warmup, "l", trace.as_ref()),
                    "lifecycle_nonet" => match &nonet_bundle {
                        Some(bundle) => lifecycle(node, bundle, n, warmup, "n", None),
                        None => Err("it needs the bundle without a network namespace, \
                                     seam_oci's eighth argument"
                            .to_owned()),
                    },
                    "start_true" => start_true(node, &true_bundle, n, warmup),
                    "throughput" => throughput(node, &true_bundle, &parent, lifecycles),
                    "sustained" => sustained(node, &true_bundle, &parent, sustained_lifecycles),
                        "interleaved" => interleaved(node, &true_bundle, &parent),
                        "list" => list(node, &sleep_bundle, n),
                        // The waits census's `client.answer.netns` row: the node's counts, asked `n` times.
                        "counts" => (0..n)
                            .try_for_each(|_| driver.netns().map(drop))
                            .map(|()| json!({ "calls": n })),
                        "serve" => serve(node, &true_bundle),
                    _ => rtt(node, &sleep_bundle, n, warmup),
                }?;
                let rose = rise(before, driver.netns()?);
                let seeds_rose = match (seeds_before, driver.seeds()?) {
                    (Some(before), Some(after)) => Some(seeds_rise(before, after)),
                    _ => None,
                };
                let reuse_rose = match (reuse_before, driver.reuse()) {
                    (Some(before), Some(after)) => Some(reuse_rise(&before, &after)),
                    _ => None,
                };
                let netns_reuse_rose = match (netns_reuse_before, driver.netns_reuse()) {
                    (Some(before), Some(after)) => Some(netns_reuse_rise(&before, &after)),
                    _ => None,
                };
                if let Value::Object(fields) = &mut v {
                    fields.insert("netns".to_owned(), rose);
                    if let (Some(before), Some(after)) = (ipc_uts_before, driver.ipc_uts()?) {
                        fields.insert("ipc_uts".to_owned(), rise(before, after));
                    }
                    if let (Some(before), Some(after)) = (dev_before, driver.dev_mounts()?) {
                        fields.insert("dev_mounts".to_owned(), json!({
                            "made":after.made-before.made, "taken":after.taken-before.taken,
                            "wakes":after.wakes-before.wakes,
                            "on_path":after.on_path-before.on_path, "discarded":after.discarded-before.discarded,
                            "failed":after.failed-before.failed, "cold":after.cold-before.cold,
                            "dry":after.dry-before.dry, "disabled":after.disabled-before.disabled,
                            "after_failure":after.after_failure-before.after_failure,
                            "unpooled_missing":after.unpooled_missing-before.unpooled_missing,
                            "unpooled_devices":after.unpooled_devices-before.unpooled_devices,
                            "unpooled_duplicate":after.unpooled_duplicate-before.unpooled_duplicate,
                            "unpooled_kind":after.unpooled_kind-before.unpooled_kind,
                            "unpooled_readonly":after.unpooled_readonly-before.unpooled_readonly,
                            "unpooled_inodes":after.unpooled_inodes-before.unpooled_inodes,
                            "unpooled_key_bytes":after.unpooled_key_bytes-before.unpooled_key_bytes,
                            "unpooled_capacity":after.unpooled_capacity-before.unpooled_capacity,
                            "unpooled_user_namespace":after.unpooled_user_namespace-before.unpooled_user_namespace,
                            "failed_filesystem":after.failed_filesystem-before.failed_filesystem,
                            "failed_device":after.failed_device-before.failed_device,
                            "failed_link":after.failed_link-before.failed_link,
                            "held_after":after.made-after.taken-after.discarded,
                        }));
                    }
                    if let (Some((before, ways_before)), Some((after, ways))) =
                        (proc_before, driver.proc_mounts()?)
                    {
                        fields.insert("proc_mounts".to_owned(), json!({
                            "made":after.made-before.made, "taken":after.taken-before.taken,
                            "on_path":after.on_path-before.on_path, "discarded":after.discarded-before.discarded,
                            "failed":after.failed-before.failed, "cold":after.cold-before.cold,
                            "dry":after.dry-before.dry, "disabled":after.disabled-before.disabled,
                            "after_failure":after.after_failure-before.after_failure,
                            "unpooled_missing":after.unpooled_missing-before.unpooled_missing,
                            "unpooled_kind":after.unpooled_kind-before.unpooled_kind,
                            "unpooled_key_bytes":after.unpooled_key_bytes-before.unpooled_key_bytes,
                            "unpooled_capacity":after.unpooled_capacity-before.unpooled_capacity,
                            "unpooled_pid_namespace":after.unpooled_pid_namespace-before.unpooled_pid_namespace,
                            "unpooled_beneath":after.unpooled_beneath-before.unpooled_beneath,
                            "failed_filesystem":after.failed_filesystem-before.failed_filesystem,
                            "failed_namespace":after.failed_namespace-before.failed_namespace,
                            "held_after":after.made-after.taken-after.discarded,
                        }));
                        fields.insert("mount_namespaces".to_owned(), json!({
                            "copy":ways[0]-ways_before[0], "empty":ways[1]-ways_before[1],
                            "tree":ways[2]-ways_before[2],
                        }));
                    }
                    if let (Some(before), Some(after)) = (roots_before, driver.roots()) {
                        fields.insert("roots".to_owned(), json!({
                            "classic":after[0]-before[0], "pruned":after[1]-before[1],
                        }));
                    }
                    if let Some(seeds_rose) = seeds_rose {
                        fields.insert("seeds".to_owned(), seeds_rose);
                    }
                    if let Some(reuse_rose) = reuse_rose {
                        fields.insert("cgroups".to_owned(), reuse_rose);
                    }
                    if let Some(netns_reuse_rose) = netns_reuse_rose {
                        fields.insert("netns_reuse".to_owned(), netns_reuse_rose);
                    }
                    if owner.is_some() {
                        fields.insert(
                            "idle_residual_bytes".to_owned(),
                            json!(idle_residuals(&root.join(hex_node::OCI_BOXES))),
                        );
                    }
                }
                Ok(v)
            });
            let note = r
                .as_ref()
                .err()
                .map(|e| format!(" ERROR {e}"))
                .unwrap_or_default();
            println!(
                "{runtime} {phase}: {:.0}s{note}",
                t0.elapsed().as_secs_f64()
            );
            match r {
                Ok(v) => phases.insert(phase.to_owned(), v),
                Err(e) => errors.insert(phase.to_owned(), Value::String(e)),
            };
        }
        // The waits census's `client.answer.hello` row: `n` connections to the socket's agent, each its hello.
        if wanted.split_whitespace().any(|w| w == "hellos") {
            let socket = state.path().join("node.sock");
            let r = match &driver {
                Driver::Socket { .. } => (0..n).try_for_each(|_| {
                    Client::connect(&socket)
                        .map(drop)
                        .map_err(|e| format!("connecting to {}: {e}", socket.display()))
                }),
                Driver::InProcess(_) => {
                    Err("the hellos arm needs HEXBENCH_DRIVER=socket".to_owned())
                }
            };
            match r {
                Ok(()) => phases.insert("hellos".to_owned(), json!({ "connections": n })),
                Err(e) => errors.insert("hellos".to_owned(), Value::String(e)),
            };
        }
        // The restart arm comes after every phase, as it replaces the socket's agent; `HEXBENCH_RESTARTS` times (once
        // unset), so the waits census's `node.listing` row has its recoveries' listings.
        let mut driver = driver;
        if wanted.split_whitespace().any(|w| w == "restart") {
            let restarts = std::env::var("HEXBENCH_RESTARTS").map_or(1, |v| {
                v.trim()
                    .parse()
                    .unwrap_or_else(|e| panic!("HEXBENCH_RESTARTS={v:?}: {e}"))
            });
            let mut runs = Vec::with_capacity(restarts);
            let t0 = Instant::now();
            for _ in 0..restarts {
                let (restarted, r) = restart(driver, hex, state.path(), &root, &sleep_bundle, n);
                driver = restarted;
                match r {
                    Ok(v) => runs.push(v),
                    Err(e) => {
                        errors.insert("restart".to_owned(), Value::String(e));
                        break;
                    }
                }
            }
            println!("{runtime} restart: {:.0}s", t0.elapsed().as_secs_f64());
            phases.insert("restart".to_owned(), Value::Array(runs));
        }
        // The idle cgroups an owner's boxes left go before the box root does; a socket agent's are its
        // next start's to remove (T-T7.1.2).
        if let Driver::InProcess(node) = &driver
            && let Err(e) = node.release_idle()
        {
            errors.insert("release_idle".to_owned(), Value::String(e.to_string()));
        }
        // The socket's node says its parked procs' counts as it ends on SIGTERM: this run's own.
        let mut parked_at_exit = Value::Null;
        if let Driver::Socket { client, agent } = driver {
            drop(client);
            match agent.end() {
                Ok(counts) => parked_at_exit = counts,
                Err(e) => {
                    errors.insert("parked_at_exit".to_owned(), Value::String(e));
                }
            }
        } else {
            drop(driver);
        }
        let _ = std::fs::remove_dir(root.join(hex_node::OCI_BOXES));
        let _ = std::fs::remove_dir(&root);
        let result = json!({"runtime": runtime, "n": n, "warmup": warmup,
                            "owner": owner.map(|o| o.name),
                            "phases": phases, "errors": errors, "cgroup": parent,
                            "parked_at_exit": parked_at_exit});
        std::fs::write(out, result.to_string()).expect("writing the result");
        ExitCode::SUCCESS
    }
}
