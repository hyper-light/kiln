//! How long a shard waits for its CPU once another thread wakes it, beside threads spinning in user space at
//! `cpu.weight` 1 on its CPUs, by each means a thread may take on Linux without privilege (kiln's goal 8, plan
//! 01a1a59c2b29 step 7; kiln's docs/runtime-lab.md, "Goal 8 step 7 and goal 9 step 4").
//!
//! A waker thread on a CPU of its own sends the shard a task (`runtime::submit_to_holder`, the path a message from
//! off the runtime takes: the shard's control queue, and its kick when it's parked) at random times, gaps drawn
//! exponential at `--gap-us` on average, one wake out at a time; the task notes when it first ran. A wake's
//! latency is from just before the send to that first poll. Each arm runs `--secs` seconds in a runtime of its own
//! (one shard, kiln's server configuration: its 266.7 µs step, a 100 µs timer tick and the shard's timer slack that
//! tick), and the arms take turns round by round, in reverse every other round, so drift on the host falls on all.
//!
//! The arms (`--arms`, by name; all by default):
//! - `calm`: no hogs; the shard asks for kiln's slice, its step (266.7 µs): the baseline.
//! - `calm-tick:US`: no hogs; kiln's slice and a nudge of `US` µs: what the nudge costs a calm machine.
//! - `plain`: hogs; the shard at the kernel's slice.
//! - `slice`: hogs; kiln's slice: what kiln's shards run at today.
//! - `slice-min`: hogs; the least slice the kernel takes, 0.1 ms (`__setparam_fair`).
//! - `idle-hogs`: the hogs at the idle policy (`SCHED_IDLE`); kiln's slice.
//! - `rr`: hogs; the shard in the round-robin real-time class at priority 1, where this process may take it
//!   (`RLIMIT_RTPRIO`; else said and skipped).
//! - `spin:US`: hogs; kiln's slice, and the shard spins `US` µs after each wake's task before it parks (the
//!   runtime's `spin_ns`, its window opened by the task as a request's does).
//! - `tick:US`: hogs; kiln's slice and a nudge of `US` µs (`compute::nudge_current_thread`, what `[server] nudge`
//!   gives each shard at the timer tick, 100 µs).
//! - `unpinned`: hogs; kiln's slice, the shard free over `--cpus` (the lab's `hogs` layout), where every other arm
//!   fixes it to the first of them.
//! - `llc`: hogs; kiln's slice, the waker on a CPU of another last-level cache where the machine has one (else said
//!   and skipped): a wake from another cache goes through the target's wakelist and its interrupt.
//!
//! Each arm and round prints a line of JSON: the wakes' percentiles (µs), how many and how many were lost (not run
//! within a second), the shard thread's CPU (% of one CPU), its driver waits a second, its run-queue wait (% of the
//! time), the hogs' CPU and the CPU on `--cpus` that was neither the shard's nor the hogs' (other work there holds
//! a wake for a tick too), both in CPUs' worth. Then a table of each arm's medians over its rounds.
//!
//! Production's layout: this process in a scope at `CPUWeight=1000` as `kiln.slice` gives the server, and the hogs
//! in a scope of their own at `CPUWeight=1` as legs and agents run (`systemd-run --user --scope`, started by this
//! bench for each arm with hogs and killed by its PID as the arm ends; `--no-scope` starts them without one, at the
//! bench's own weight, and says so). Build it, then run it in its scope:
//!
//! ```text
//! cargo bench -p hyper-rt --bench lazy_wake --no-run
//! systemd-run --user --scope --quiet --collect -p CPUWeight=1000 -- \
//!   cargo bench -p hyper-rt --bench lazy_wake -- --cpus 16-19 --waker 24 --secs 10 --rounds 3
//! ```
#![allow(
    unsafe_code,
    missing_docs,
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::exit,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::disallowed_methods,
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss,
    clippy::cast_precision_loss,
    clippy::cognitive_complexity,
    clippy::missing_panics_doc,
    clippy::too_many_lines
)]

#[cfg(not(target_os = "linux"))]
fn main() {
    println!("lazy_wake measures Linux's lazy preemption: there is nothing of it to measure on this OS");
}

#[cfg(target_os = "linux")]
fn main() {
    linux::main();
}

#[cfg(target_os = "linux")]
mod linux {
    use std::process::{Child, Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use hyper_rt::SlotHolder;
    use hyper_rt::compute;
    use hyper_rt::runtime::{Runtime, RuntimeConfig, submit_to_holder};

    /// kiln's server's step, (1 ms − 200 µs) / 3, and timer tick (crates/kiln/src/rt.rs, `STEP_BUDGET_NS` and
    /// `TIMER_TICK_NS`): the slice its shards ask for, and the nudge's period `[server] nudge` gives them.
    const STEP_NS: u64 = 266_666;
    const TICK_NS: u64 = 100_000;
    /// The least slice the kernel takes for the normal policy (Linux 6.19's `__setparam_fair`: 0.1 ms).
    const SLICE_MIN_NS: u64 = 100_000;
    /// A wake whose task hasn't run in this long is lost: far past any tick.
    const LOST_NS: u64 = 1_000_000_000;
    /// Wakes of each arm not counted: its first, while caches and the shard's arenas warm.
    const WARM: usize = 50;
    /// The arms, in order.
    const ARMS: &str = "calm,calm-tick:100,plain,slice,slice-min,idle-hogs,rr,spin:10,spin:100,tick:100,tick:50,unpinned,llc";

    fn now_ns() -> u64 {
        let t = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        t.tv_sec as u64 * 1_000_000_000 + t.tv_nsec as u64
    }

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Arm {
        Calm,
        CalmTick(u64),
        Plain,
        Slice,
        SliceMin,
        IdleHogs,
        Rr,
        Spin(u64),
        Tick(u64),
        Unpinned,
        Llc,
    }

    impl Arm {
        fn parse(name: &str) -> Option<Arm> {
            let (head, us) = match name.split_once(':') {
                Some((head, us)) => (head, Some(us.parse::<u64>().ok()?)),
                None => (name, None),
            };
            Some(match (head, us) {
                ("calm", None) => Arm::Calm,
                ("calm-tick", Some(us)) => Arm::CalmTick(us),
                ("plain", None) => Arm::Plain,
                ("slice", None) => Arm::Slice,
                ("slice-min", None) => Arm::SliceMin,
                ("idle-hogs", None) => Arm::IdleHogs,
                ("rr", None) => Arm::Rr,
                ("spin", Some(us)) => Arm::Spin(us),
                ("tick", Some(us)) => Arm::Tick(us),
                ("unpinned", None) => Arm::Unpinned,
                ("llc", None) => Arm::Llc,
                _ => return None,
            })
        }

        fn name(&self) -> String {
            match self {
                Arm::Calm => "calm".into(),
                Arm::CalmTick(us) => format!("calm-tick:{us}"),
                Arm::Plain => "plain".into(),
                Arm::Slice => "slice".into(),
                Arm::SliceMin => "slice-min".into(),
                Arm::IdleHogs => "idle-hogs".into(),
                Arm::Rr => "rr".into(),
                Arm::Spin(us) => format!("spin:{us}"),
                Arm::Tick(us) => format!("tick:{us}"),
                Arm::Unpinned => "unpinned".into(),
                Arm::Llc => "llc".into(),
            }
        }

        /// The hogs' kind, or none.
        fn hogs(&self) -> Option<&'static str> {
            match self {
                Arm::Calm | Arm::CalmTick(_) => None,
                Arm::IdleHogs => Some("idle"),
                _ => Some("spin"),
            }
        }

        fn spin_ns(&self) -> u64 {
            match self {
                Arm::Spin(us) => us * 1000,
                _ => 0,
            }
        }

        fn pinned(&self) -> bool {
            *self != Arm::Unpinned
        }

        /// What the shard asks of the OS on its own thread, as kiln's shards ask as they start: what refused it.
        fn ask(&self) -> Option<String> {
            let said = |r: compute::Refusal| r.to_string();
            match self {
                Arm::Plain => None,
                Arm::Rr => compute::realtime_current_thread(compute::REALTIME_LOWEST).err().map(said),
                Arm::SliceMin => compute::prefer_current_thread(SLICE_MIN_NS).err().map(said),
                Arm::Tick(us) | Arm::CalmTick(us) => {
                    let slice = compute::prefer_current_thread(STEP_NS).err();
                    let nudge = compute::nudge_current_thread(us * 1000).err();
                    slice.or(nudge).map(said)
                }
                _ => compute::prefer_current_thread(STEP_NS).err().map(said),
            }
        }
    }

    struct Options {
        cpus: Vec<usize>,
        hogs: usize,
        waker: usize,
        secs: f64,
        rounds: usize,
        gap_us: u64,
        arms: Vec<Arm>,
        scope: bool,
    }

    /// A CPU list as the kernel writes one (`16-19,24`).
    fn cpu_list(text: &str) -> Vec<usize> {
        let mut cpus = Vec::new();
        for part in text.trim().split(',').filter(|p| !p.is_empty()) {
            match part.split_once('-') {
                Some((a, b)) => {
                    if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) {
                        cpus.extend(a..=b);
                    }
                }
                None => cpus.extend(part.parse::<usize>().ok()),
            }
        }
        cpus
    }

    fn list_of(cpus: &[usize]) -> String {
        cpus.iter().map(ToString::to_string).collect::<Vec<_>>().join(",")
    }

    fn read(path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn allowed() -> Vec<usize> {
        let set = rustix::thread::sched_getaffinity(None).unwrap();
        (0..rustix::thread::CpuSet::MAX_CPU).filter(|&cpu| set.is_set(cpu)).collect()
    }

    fn pin(cpus: &[usize]) {
        let mut set = rustix::thread::CpuSet::new();
        for &cpu in cpus {
            set.set(cpu);
        }
        rustix::thread::sched_setaffinity(None, &set).unwrap();
    }

    fn siblings(cpu: usize) -> Vec<usize> {
        read(&format!("/sys/devices/system/cpu/cpu{cpu}/topology/thread_siblings_list")).map_or_else(|| vec![cpu], |s| cpu_list(&s))
    }

    /// The CPUs sharing `cpu`'s last-level cache: its highest-level cache's sharers.
    fn llc_of(cpu: usize) -> String {
        let mut best = (0u32, String::new());
        for index in 0..8 {
            let base = format!("/sys/devices/system/cpu/cpu{cpu}/cache/index{index}");
            let (Some(level), Some(shared)) = (read(&format!("{base}/level")), read(&format!("{base}/shared_cpu_list"))) else {
                continue;
            };
            let level = level.trim().parse::<u32>().unwrap_or(0);
            if level >= best.0 {
                best = (level, shared.trim().to_owned());
            }
        }
        best.1
    }

    fn options(args: &[String]) -> Options {
        let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
        let allowed = allowed();
        let cpus = value("--cpus").map_or_else(|| allowed.iter().rev().take(4).rev().copied().collect(), |s| cpu_list(&s));
        assert!(!cpus.is_empty(), "no CPUs for the shard");
        let near: Vec<usize> = cpus.iter().flat_map(|&c| siblings(c)).collect();
        let waker = value("--waker").and_then(|s| s.parse().ok()).unwrap_or_else(|| {
            allowed.iter().rev().copied().find(|c| !near.contains(c)).expect("a CPU for the waker that isn't the shard's or a sibling of one")
        });
        let arms = value("--arms").unwrap_or_else(|| ARMS.to_owned());
        let arms = arms.split(',').map(|a| Arm::parse(a.trim()).unwrap_or_else(|| panic!("no arm {a:?}: {ARMS}"))).collect();
        Options {
            hogs: value("--hogs").and_then(|s| s.parse().ok()).unwrap_or(cpus.len()),
            cpus,
            waker,
            secs: value("--secs").and_then(|s| s.parse().ok()).unwrap_or(10.0),
            rounds: value("--rounds").and_then(|s| s.parse().ok()).unwrap_or(3),
            gap_us: value("--gap-us").and_then(|s| s.parse().ok()).unwrap_or(1000),
            arms,
            scope: !args.iter().any(|a| a == "--no-scope"),
        }
    }

    /// The kernel and machine, as the lab notes them (`scripts/priority_lab`): its preemption, tick, caches,
    /// sched_ext, the real-time grant and this process's cgroup weight.
    fn facts(o: &Options) {
        let uname = rustix::system::uname();
        let release = uname.release().to_string_lossy().into_owned();
        let config = read(&format!("/boot/config-{release}"))
            .or_else(|| read(&format!("/lib/modules/{release}/config")))
            .unwrap_or_default();
        let config: Vec<&str> = config
            .lines()
            .filter(|l| l.starts_with("CONFIG_PREEMPT") || l.starts_with("CONFIG_HZ=") || l.starts_with("CONFIG_SCHED_CLASS_EXT"))
            .collect();
        let cmdline = read("/proc/cmdline").unwrap_or_default();
        let preempt_arg: Vec<&str> = cmdline.split_whitespace().filter(|w| w.starts_with("preempt=")).collect();
        let debugfs = read("/sys/kernel/debug/sched/preempt").map_or_else(|| "unreadable (root's)".to_owned(), |s| s.trim().to_owned());
        let mut llcs: Vec<String> = allowed().into_iter().map(llc_of).collect();
        llcs.sort();
        llcs.dedup();
        let scx = read("/sys/kernel/sched_ext/state").map_or_else(|| "none".to_owned(), |s| s.trim().to_owned());
        let rtprio = rustix::process::getrlimit(rustix::process::Resource::Rtprio);
        let cgroup = read("/proc/self/cgroup").unwrap_or_default();
        let path = cgroup.lines().find_map(|l| l.strip_prefix("0::")).unwrap_or("").trim().to_owned();
        let weight = read(&format!("/sys/fs/cgroup{path}/cpu.weight")).map_or_else(|| "unreadable".to_owned(), |s| s.trim().to_owned());
        println!("kernel {release}; {}; preempt= on the command line: {preempt_arg:?}; debugfs: {debugfs}", config.join(" "));
        println!("last-level caches: {llcs:?}; sched_ext: {scx}; RLIMIT_RTPRIO {:?} soft, {:?} hard", rtprio.current, rtprio.maximum);
        println!("this process: cgroup {path} at cpu.weight {weight}; hogs {}", if o.scope { "in a scope of their own at CPUWeight=1" } else { "unscoped, at this process's weight (--no-scope)" });
        println!(
            "the shard on {:?} ({} hogs there), the waker on {}; {} s an arm, {} rounds, gaps of {} µs on average",
            o.cpus, o.hogs, o.waker, o.secs, o.rounds, o.gap_us
        );
    }

    /// The hogs' process: `n` threads fixed round-robin to `cpus`, spinning in user space and never entering the
    /// kernel, at the idle policy for `idle`; they die with the bench.
    fn hog(kind: &str, cpus: &[usize], n: usize) {
        // SAFETY: no pointer passes; the call asks for SIGKILL when this process's parent ends (prctl(2)).
        unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL as libc::c_ulong) };
        let idle = kind == "idle";
        let threads: Vec<_> = (0..n)
            .map(|i| {
                let cpu = cpus[i % cpus.len()];
                std::thread::spawn(move || {
                    pin(&[cpu]);
                    if idle {
                        let param = libc::sched_param { sched_priority: 0 };
                        // SAFETY: a live sched_param read for the call; pid 0 is the calling thread.
                        unsafe { libc::sched_setscheduler(0, libc::SCHED_IDLE, &raw const param) };
                    }
                    let mut x: u64 = cpu as u64 | 1;
                    loop {
                        x = std::hint::black_box(x.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407));
                    }
                })
            })
            .collect();
        for thread in threads {
            let _ = thread.join();
        }
    }

    fn start_hogs(kind: &str, o: &Options, seq: usize) -> Child {
        let exe = std::env::current_exe().unwrap();
        let mut command = if o.scope {
            let mut c = Command::new("systemd-run");
            c.args(["--user", "--scope", "--quiet", "--collect", "-p", "CPUWeight=1"])
                .arg(format!("--unit=lazy-wake-hogs-{}-{seq}", std::process::id()))
                .arg("--")
                .arg(exe);
            c
        } else {
            Command::new(exe)
        };
        command
            .args(["--hog", kind, "--cpus", &list_of(&o.cpus), "--n", &o.hogs.to_string()])
            .stdin(Stdio::null())
            .stdout(Stdio::null());
        let child = command.spawn().expect("the hogs start (systemd-run --user, or --no-scope)");
        // The hogs take their CPUs before the arm measures.
        std::thread::sleep(Duration::from_millis(500));
        child
    }

    /// A thread's CPU (ns), run-queue wait (ns) and timeslices, as the kernel counts them.
    fn schedstat(path: &str) -> (u64, u64, u64) {
        let text = read(path).unwrap_or_default();
        let mut v = text.split_whitespace().map(|w| w.parse::<u64>().unwrap_or(0));
        (v.next().unwrap_or(0), v.next().unwrap_or(0), v.next().unwrap_or(0))
    }

    /// The CPU every thread of process `pid` used, ns.
    fn process_cpu(pid: u32) -> u64 {
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
            return 0;
        };
        tasks.filter_map(Result::ok).map(|t| schedstat(&format!("{}/schedstat", t.path().display())).0).sum()
    }

    /// The busy time of `cpus` (all but idle and I/O wait), ns, from /proc/stat's ticks.
    fn busy_ns(cpus: &[usize]) -> u64 {
        let ticks_ns = 1_000_000_000 / rustix::param::clock_ticks_per_second().max(1);
        let stat = read("/proc/stat").unwrap_or_default();
        stat.lines()
            .filter_map(|line| {
                let mut words = line.split_whitespace();
                let cpu: usize = words.next()?.strip_prefix("cpu")?.parse().ok()?;
                if !cpus.contains(&cpu) {
                    return None;
                }
                let v: Vec<u64> = words.map(|w| w.parse().unwrap_or(0)).collect();
                // user nice system idle iowait irq softirq steal (guest is within user).
                Some([0, 1, 2, 5, 6, 7].iter().map(|&i| v.get(i).copied().unwrap_or(0)).sum::<u64>() * ticks_ns)
            })
            .sum()
    }

    struct Rng(u64);

    impl Rng {
        fn unit(&mut self) -> f64 {
            let mut x = self.0;
            x ^= x >> 12;
            x ^= x << 25;
            x ^= x >> 27;
            self.0 = x;
            ((x.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 11) as f64 + 1.0) / (1u64 << 53) as f64
        }
    }

    /// The waker on `cpu`: wakes for `secs`, gaps exponential at `gap_us`; the latencies (ns) and the wakes lost.
    fn wakes(holder: SlotHolder, cpu: usize, secs: f64, gap_us: u64, seed: u64, opens_window: bool) -> (Vec<u64>, u64) {
        std::thread::spawn(move || {
            pin(&[cpu]);
            let mut rng = Rng(seed | 1);
            let mean = gap_us as f64 * 1000.0;
            let end = now_ns() + (secs * 1e9) as u64;
            let (mut latencies, mut lost) = (Vec::new(), 0u64);
            while now_ns() < end {
                let gap = (-mean * rng.unit().ln()).min(20.0 * mean) as u64;
                std::thread::sleep(Duration::from_nanos(gap));
                let ran = Arc::new(AtomicU64::new(0));
                let mark = Arc::clone(&ran);
                let sent = now_ns();
                let task = async move {
                    mark.store(now_ns(), Ordering::Release);
                    if opens_window {
                        hyper_rt::registry::with_current(|ctx| ctx.note_activity());
                    }
                };
                if submit_to_holder(holder, task).is_err() {
                    lost += 1;
                    continue;
                }
                loop {
                    let at = ran.load(Ordering::Acquire);
                    if at != 0 {
                        latencies.push(at.saturating_sub(sent));
                        break;
                    }
                    if now_ns() > sent + LOST_NS {
                        lost += 1;
                        break;
                    }
                    std::hint::spin_loop();
                }
            }
            (latencies.split_off(WARM.min(latencies.len())), lost)
        })
        .join()
        .unwrap()
    }

    struct Row {
        arm: String,
        skipped: Option<String>,
        /// p50, p90, p99, p99.9 and max, µs.
        at: [f64; 5],
        samples: usize,
        lost: u64,
        shard_cpu_pct: f64,
        waits_per_s: f64,
        run_delay_pct: f64,
        hogs_cpus: f64,
        foreign_cpus: f64,
        note: Option<String>,
    }

    fn json(round: usize, r: &Row) -> String {
        match &r.skipped {
            Some(why) => format!("{{\"round\":{round},\"arm\":\"{}\",\"skipped\":{why:?}}}", r.arm),
            None => format!(
                "{{\"round\":{round},\"arm\":\"{}\",\"p50_us\":{:.1},\"p90_us\":{:.1},\"p99_us\":{:.1},\"p999_us\":{:.1},\"max_us\":{:.1},\"samples\":{},\"lost\":{},\"shard_cpu_pct\":{:.2},\"waits_per_s\":{:.0},\"run_delay_pct\":{:.2},\"hogs_cpus\":{:.3},\"foreign_cpus\":{:.3},\"note\":{:?}}}",
                r.arm, r.at[0], r.at[1], r.at[2], r.at[3], r.at[4], r.samples, r.lost, r.shard_cpu_pct, r.waits_per_s, r.run_delay_pct, r.hogs_cpus, r.foreign_cpus, r.note.as_deref().unwrap_or("")
            ),
        }
    }

    fn skipped(arm: &Arm, why: String) -> Row {
        Row { arm: arm.name(), skipped: Some(why), at: [0.0; 5], samples: 0, lost: 0, shard_cpu_pct: 0.0, waits_per_s: 0.0, run_delay_pct: 0.0, hogs_cpus: 0.0, foreign_cpus: 0.0, note: None }
    }

    fn measure(arm: &Arm, o: &Options, llc_waker: Option<usize>, seq: usize) -> Row {
        let waker = match arm {
            Arm::Llc => match llc_waker {
                Some(cpu) => cpu,
                None => return skipped(arm, "one last-level cache here: every wake is within it".into()),
            },
            _ => o.waker,
        };
        let config = RuntimeConfig {
            shards: 1,
            tasks_per_shard: 64,
            timers_per_shard: 128,
            interests_per_shard: 128,
            ring_entries: 1024,
            step_budget_ns: STEP_NS,
            timer_tick_ns: TICK_NS,
            batch: 64,
            pin: arm.pinned(),
            cores: if arm.pinned() { vec![o.cpus[0] as u32] } else { Vec::new() },
            page_bytes: 4096,
            spin_ns: arm.spin_ns(),
            wake_tracking: None,
            latency_objective_ns: None,
        };
        let hogs = arm.hogs().map(|kind| start_hogs(kind, o, seq));
        // An unpinned shard takes the CPUs of the thread that starts it.
        if !arm.pinned() {
            pin(&o.cpus);
        }
        let runtime = Runtime::start(&config).expect("the runtime starts");
        pin(&[o.waker]);
        let holder = runtime.shard_ids().first().and_then(|id| runtime.holder_of(*id)).expect("a shard");
        let (told, answer) = std::sync::mpsc::channel();
        let asks = arm.clone();
        submit_to_holder(holder, async move {
            let tid = rustix::thread::gettid().as_raw_nonzero().get();
            let _ = told.send((tid, asks.ask()));
        })
        .expect("the shard takes its asks");
        let (tid, refused) = answer.recv_timeout(Duration::from_secs(5)).expect("the shard answers");
        let stop = |runtime: Runtime, hogs: Option<Child>| {
            let counters = runtime.shutdown().unwrap_or_default();
            if let Some(mut hogs) = hogs {
                let _ = hogs.kill();
                let _ = hogs.wait();
            }
            counters
        };
        if *arm == Arm::Rr
            && let Some(why) = refused
        {
            let _ = stop(runtime, hogs);
            return skipped(arm, why);
        }
        let shard_stat = format!("/proc/self/task/{tid}/schedstat");
        let hog_pid = hogs.as_ref().map(Child::id);
        let (cpu0, delay0, _) = schedstat(&shard_stat);
        let hogs0 = hog_pid.map_or(0, process_cpu);
        let busy0 = busy_ns(&o.cpus);
        let began = now_ns();
        let (mut latencies, lost) = wakes(holder, waker, o.secs, o.gap_us, 0x9E37_79B9 ^ seq as u64, matches!(arm, Arm::Spin(_)));
        let wall = (now_ns() - began) as f64;
        let (cpu1, delay1, _) = schedstat(&shard_stat);
        let hogs1 = hog_pid.map_or(0, process_cpu);
        let busy1 = busy_ns(&o.cpus);
        let counters = stop(runtime, hogs);
        latencies.sort_unstable();
        let at = |q: f64| latencies.get(((latencies.len() as f64 - 1.0) * q).round() as usize).map_or(0.0, |&ns| ns as f64 / 1e3);
        let shard_ns = cpu1.saturating_sub(cpu0);
        let hogs_ns = hogs1.saturating_sub(hogs0);
        Row {
            arm: arm.name(),
            skipped: None,
            at: [at(0.5), at(0.9), at(0.99), at(0.999), at(1.0)],
            samples: latencies.len(),
            lost,
            shard_cpu_pct: shard_ns as f64 / wall * 100.0,
            waits_per_s: counters.first().map_or(0.0, |c| c.waits as f64 / (wall / 1e9)),
            run_delay_pct: delay1.saturating_sub(delay0) as f64 / wall * 100.0,
            hogs_cpus: hogs_ns as f64 / wall,
            foreign_cpus: busy1.saturating_sub(busy0).saturating_sub(shard_ns).saturating_sub(hogs_ns) as f64 / wall,
            note: refused,
        }
    }

    fn median(mut v: Vec<f64>) -> f64 {
        v.sort_by(f64::total_cmp);
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    }

    pub(super) fn main() {
        let args: Vec<String> = std::env::args().skip(1).collect();
        if let Some(i) = args.iter().position(|a| a == "--hog") {
            let value = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
            let kind = args.get(i + 1).cloned().unwrap_or_else(|| "spin".into());
            hog(&kind, &cpu_list(&value("--cpus").unwrap_or_default()), value("--n").and_then(|n| n.parse().ok()).unwrap_or(1));
            std::process::exit(0);
        }
        let o = options(&args);
        pin(&[o.waker]);
        facts(&o);
        let home = llc_of(o.cpus[0]);
        let llc_waker = allowed().into_iter().rev().find(|&c| !o.cpus.contains(&c) && llc_of(c) != home);
        let mut rows: Vec<Vec<Row>> = o.arms.iter().map(|_| Vec::new()).collect();
        let mut seq = 0;
        for round in 1..=o.rounds {
            let order: Vec<usize> = if round % 2 == 1 { (0..o.arms.len()).collect() } else { (0..o.arms.len()).rev().collect() };
            for i in order {
                seq += 1;
                let row = measure(&o.arms[i], &o, llc_waker, seq);
                println!("{}", json(round, &row));
                rows[i].push(row);
            }
        }
        println!();
        println!(
            "{:<14} {:>9} {:>9} {:>10} {:>10} {:>11} {:>9} {:>10} {:>10} {:>12} {:>8}",
            "arm", "p50 µs", "p99 µs", "p99.9 µs", "max µs", "shard CPU%", "waits/s", "run-wait%", "hogs CPUs", "foreign CPUs", "samples"
        );
        for (arm, rows) in o.arms.iter().zip(&rows) {
            if let Some(why) = rows.iter().find_map(|r| r.skipped.clone()) {
                println!("{:<14} skipped: {why}", arm.name());
                continue;
            }
            let m = |f: &dyn Fn(&Row) -> f64| median(rows.iter().map(f).collect());
            println!(
                "{:<14} {:>9.1} {:>9.1} {:>10.1} {:>10.1} {:>11.2} {:>9.0} {:>10.2} {:>10.3} {:>12.3} {:>8}",
                arm.name(),
                m(&|r| r.at[0]),
                m(&|r| r.at[2]),
                m(&|r| r.at[3]),
                rows.iter().map(|r| r.at[4]).fold(0.0, f64::max),
                m(&|r| r.shard_cpu_pct),
                m(&|r| r.waits_per_s),
                m(&|r| r.run_delay_pct),
                m(&|r| r.hogs_cpus),
                m(&|r| r.foreign_cpus),
                rows.iter().map(|r| r.samples).sum::<usize>(),
            );
        }
    }
}
