//! The macOS `process` class's own path, measured (native plan step 1, AC-T7.1.5-style accounting): how
//! far a box's footprint passes its 256 MiB memory bound before its watcher ends it, with 1, 4, 8, 14 and
//! 28 members allocating at full speed, and the watcher's CPU for a box held at 95% of its bound. Both go
//! through [`hex_runtime::macos::MacRuntime`] and real `hex selftest-box` members, so the figure is the
//! class's, not a standalone probe's (bench/native/macos/box_probe.c measures the raw interfaces).
//!
//!   macos_box [passes]           both measurements, `passes` times each (default 3)
//!   macos_box readcost [passes]  the watcher's read-cost curve: CPU against reads a second, by box size
//!   macos_box events [passes]    the most events one kqueue wait takes, by box size
//!   macos_box fill [passes]      a box's worst aggregate growth: one, two and four members per core
//!                                filling memory, and spawning, all at once
//!
//! `readcost` and `events` size the watcher's ceiling and its event batch (the lead's calls 1 and 2,
//! 2026-11-06, under Ada's rule): each drives the watcher's own calls (`hex_runtime::macos::sys`) on a group
//! of `/bin/sleep` members, so nothing else in the process spends CPU or takes events.
//!
//! The record is `bench/native/macos/RESULT.md`; run it on adas-mac-studio-3 and capture its rows.
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros,
    clippy::print_stdout,
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("SKIP: the macOS box bench is macOS-only");
}

#[cfg(target_os = "macos")]
fn main() {
    macos::main();
}

#[cfg(target_os = "macos")]
mod macos {
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::{Child, Command, Stdio as Piped};
    use std::time::{Duration, Instant};

    use hex_api::{Bounds, BoxId, BoxSpec, ClassSpec, Program, Runtime, Stdio};
    use hex_core::watch::{lateness_seed, seed};
    use hex_runtime::macos::sys;
    use hex_runtime::macos::{MacRuntime, machine_model};

    const MIB: u64 = 1 << 20;
    const GIB: u64 = 1 << 30;
    const BOUND: u64 = 256 * MIB;

    /// The hex binary whose `selftest-box` acts are the box's members: `HEX_BIN`, else the `hex` beside
    /// this bench's own executable (cargo puts both under the same profile directory).
    fn hex_binary() -> PathBuf {
        if let Some(p) = std::env::var_os("HEX_BIN") {
            return PathBuf::from(p);
        }
        let me = std::env::current_exe().expect("current exe");
        // target/<profile>/deps/macos_box-<hash> -> target/<profile>/hex
        let hex = me
            .parent()
            .and_then(|deps| deps.parent())
            .map(|profile| profile.join("hex"))
            .expect("a profile directory above the bench");
        assert!(
            hex.exists(),
            "no hex binary at {} (set HEX_BIN)",
            hex.display()
        );
        hex
    }

    /// A runtime seeded, as a node is, with its machine class's recorded worst aggregate growth
    /// (`hex_core::watch::seed` of this Mac's `hw.model`), so each box is read before it can pass its bound
    /// by the 64 MiB bar, every member filling at once.
    fn runtime() -> MacRuntime {
        let model = machine_model().expect("this Mac's model");
        let rates = seed(&model);
        MacRuntime::new(rates.bytes, rates.processes, lateness_seed(&model))
            .expect("a macOS runtime")
    }

    fn member(hex: &Path, act: &[&str]) -> Program {
        let mut args = vec![
            hex.to_string_lossy().into_owned(),
            "selftest-box".to_owned(),
        ];
        args.extend(act.iter().map(|a| (*a).to_owned()));
        Program::of(&args.iter().map(String::as_str).collect::<Vec<_>>())
    }

    /// One overshoot run: a 256 MiB box with `members` hogs allocating at full speed; returns how far past
    /// the bound the box's footprint reached when the watcher ended it, in bytes.
    fn overshoot(hex: &Path, members: usize) -> u64 {
        let rt = runtime();
        let id = BoxId::new(&format!("mac-bench-over-{members}")).unwrap();
        let _ = rt.delete(&id);
        rt.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds {
                memory_max: Some(BOUND),
                ..Bounds::default()
            },
            class: ClassSpec::Process,
            owner: None,
            band: 0,
        })
        .expect("create");
        // Each member allocates far past the whole bound, so the box's summed footprint races up until the
        // watcher kills it; the members never finish allocating.
        let per = 4 * GIB;
        let mut procs = Vec::new();
        for _ in 0..members {
            procs.push(
                rt.exec(&id, member(hex, &["hog", &per.to_string()]), Stdio::null())
                    .expect("exec hog"),
            );
        }
        // Wait on the members' ends (the watcher's kill ends them all), never spinning on usage: a spinning
        // reader takes a core from the members and the watcher, and its live reads catch members still
        // growing after the kill. Then read the peak.
        let mut peak = 0;
        for p in &mut procs {
            let exit = p.wait_for(Duration::from_secs(30)).expect("wait");
            assert!(exit.is_some(), "the box never ended for memory");
            peak = peak.max(rt.usage(&id).expect("usage").memory_peak_bytes);
        }
        rt.delete(&id).expect("delete");
        drop(procs);
        let rates = rt.pacing().expect("pacing");
        println!(
            "  pacing members={members} bytes_per_s={} spawns_per_s={}",
            rates.bytes, rates.processes
        );
        peak.saturating_sub(BOUND)
    }

    /// The watcher's CPU for a box of `members` held together at `percent` of its bound for `secs` seconds:
    /// the bench process's own CPU over that window (nothing else in it runs — the main thread parks), as a
    /// percentage of one core, and whether the box held through it (the watcher didn't end it: each
    /// member's own baseline footprint adds to what it holds). One member is armed with the kernel's cap
    /// and left to its events; two or more are paced by headroom.
    fn watcher_cpu(hex: &Path, secs: u64, percent: u64, members: u64) -> (f64, bool) {
        let rt = runtime();
        let id = BoxId::new("mac-bench-hold").unwrap();
        let _ = rt.delete(&id);
        rt.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds {
                memory_max: Some(BOUND),
                ..Bounds::default()
            },
            class: ClassSpec::Process,
            owner: None,
            band: 0,
        })
        .expect("create");
        let hold = BOUND / 100 * percent;
        let millis = secs * 1000 + 4000;
        let _p: Vec<_> = (0..members)
            .map(|_| {
                rt.exec(
                    &id,
                    member(
                        hex,
                        &["hold", &(hold / members).to_string(), &millis.to_string()],
                    ),
                    Stdio::null(),
                )
                .expect("exec hold")
            })
            .collect();
        // Wait for it to hold its footprint, so the watcher paces by the headroom it holds at; then measure
        // with the main thread parked so the only CPU the process spends is the watcher's.
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let u = rt.usage(&id).expect("usage");
            // Held, or ended by the watcher (the members' baseline took it past the bound): measured either
            // way, and marked held=false.
            if u.memory_bytes >= hold / 100 * 99 || u.oom_kills >= 1 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the hold member never reached {percent}%"
            );
            std::thread::yield_now();
        }
        let c0 = sys::process_cpu().expect("process cpu");
        std::thread::park_timeout(Duration::from_secs(secs));
        let cpu = sys::process_cpu().expect("process cpu").saturating_sub(c0);
        let held = rt.usage(&id).expect("usage").oom_kills == 0;
        let rates = rt.pacing().expect("pacing");
        println!(
            "  pacing members={members} hold_pct_of_bound={percent} bytes_per_s={} spawns_per_s={}",
            rates.bytes, rates.processes
        );
        rt.delete(&id).expect("delete");
        (cpu.as_secs_f64() / secs as f64 * 100.0, held)
    }

    /// The box sizes the overshoot pass measures: one member up to four times the cores of the record's Mac.
    const COUNTS: [usize; 6] = [1, 4, 8, 14, 28, 56];

    /// `members` `/bin/sleep` processes in one new process group, the first its leader; returns them and
    /// the group's id.
    fn sleepers(members: usize) -> (Vec<Child>, i32) {
        let mut kids: Vec<Child> = Vec::new();
        let mut pgid = 0;
        for _ in 0..members {
            let child = Command::new("/bin/sleep")
                .arg("600")
                .stdin(Piped::null())
                .stdout(Piped::null())
                .stderr(Piped::null())
                .process_group(pgid)
                .spawn()
                .expect("spawn a sleeper");
            if pgid == 0 {
                pgid = i32::try_from(child.id()).unwrap();
            }
            kids.push(child);
        }
        (kids, pgid)
    }

    fn end(mut kids: Vec<Child>) {
        for k in &mut kids {
            let _ = k.kill();
            let _ = k.wait();
        }
    }

    /// The watcher's read at one pace: for `secs` seconds a thread, as the watcher is,
    /// waits a one-shot `NOTE_CRITICAL` timer of `pace` and reads the group of `members` (its listing and
    /// each member's rusage), the calls a watcher read makes. Returns reads a second and the process's CPU
    /// over the window as a percentage of a core (only this thread runs).
    fn read_cost(members: usize, pace: Duration, secs: u64) -> (f64, f64) {
        let (kids, pgid) = sleepers(members);
        let window = Duration::from_secs(secs);
        let (reads, cpu) = std::thread::spawn(move || {
            let kq = sys::Kqueue::new().expect("kqueue");
            let mut pids = sys::listing().expect("listing");
            let mut events = Vec::new();
            let c0 = sys::process_cpu().expect("process cpu");
            let t0 = Instant::now();
            let mut reads = 0u64;
            while t0.elapsed() < window {
                kq.wait(Some(pace), None, &mut events).expect("wait");
                sys::group(pgid, &mut pids).expect("group");
                for &pid in &pids {
                    let _ = sys::rusage(pid).expect("rusage");
                }
                reads += 1;
            }
            let cpu = sys::process_cpu().expect("process cpu").saturating_sub(c0);
            (
                reads as f64 / t0.elapsed().as_secs_f64(),
                cpu.as_secs_f64() / t0.elapsed().as_secs_f64(),
            )
        })
        .join()
        .expect("the reader");
        end(kids);
        (reads, cpu * 100.0)
    }

    /// The most events one kqueue wait takes from a box of `members` whose members all end at once, the
    /// largest burst the overshoot pass makes (its watcher kills the whole group): each member watched for
    /// its forks and end as the watcher watches them, the group killed in one `kill(2)`, then
    /// - `first`: what the first wait after the kill takes, as the watcher's wake does;
    /// - `settled`: what one wait takes once every member has ended, every event pending.
    ///
    /// The wait takes at most `sys::EVENTS`, so a count at it may have left events for the next call.
    fn events_per_wait(members: usize) -> (usize, usize) {
        let mut most = (0, 0);
        for settle in [false, true] {
            let (kids, pgid) = sleepers(members);
            let pids: Vec<i32> = kids
                .iter()
                .map(|k| i32::try_from(k.id()).unwrap())
                .collect();
            let kq = sys::Kqueue::new().expect("kqueue");
            let gone = kq.watch(&pids, 0).expect("watch");
            assert!(gone.is_empty(), "a sleeper ended before it was watched");
            let killed = Command::new("/bin/kill")
                .args(["-KILL", "--", &format!("-{pgid}")])
                .status()
                .expect("kill the group");
            assert!(killed.success(), "kill -KILL -{pgid}");
            if settle {
                // Every member a zombie: each one's end is queued.
                let deadline = Instant::now() + Duration::from_secs(30);
                while !pids
                    .iter()
                    .all(|&p| sys::rusage(p).expect("rusage").is_none_or(|r| r.ended))
                {
                    assert!(Instant::now() < deadline, "the group never ended");
                    std::thread::yield_now();
                }
            }
            let mut events = Vec::new();
            kq.wait(None, Some(Duration::from_secs(30)), &mut events)
                .expect("wait");
            let n = events.len();
            if settle {
                most.1 = n;
            } else {
                most.0 = n;
            }
            end(kids);
        }
        most
    }

    /// What one fill member takes: a box's whole bound, so each races past it as the overshoot pass's hogs do.
    const FILL_BYTES: u64 = BOUND;
    /// How many `true`s one spawn member starts, one after another: step 7's growth record's n (20).
    const SPAWNS: usize = 20;

    /// A member of the `fill` pass, run as this bench's own child: waits for the go byte on stdin, then
    /// either fills `FILL_BYTES` of fresh memory with an incompressible xorshift stream a u64 at a time (as
    /// step 7's native_box measures one member's fill) or spawns and reaps `/usr/bin/true` `SPAWNS` times,
    /// and prints what it did and its own elapsed nanoseconds.
    fn member_main(what: &str) {
        let mut go = [0u8; 1];
        std::io::stdin().read_exact(&mut go).expect("the go byte");
        let t = Instant::now();
        let done = if what == "fill-member" {
            let mut touched = vec![0u8; FILL_BYTES as usize];
            let mut x = 0x9e37_79b9_7f4a_7c15u64;
            for word in touched.as_chunks_mut::<{ size_of::<u64>() }>().0 {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                *word = x.to_le_bytes();
            }
            std::hint::black_box(&touched);
            FILL_BYTES
        } else {
            for _ in 0..SPAWNS {
                Command::new("/usr/bin/true")
                    .stdout(Piped::null())
                    .status()
                    .expect("spawn true");
            }
            SPAWNS as u64
        };
        println!("{done} {}", t.elapsed().as_nanos());
    }

    /// One `fill` pass of `what` (`fill-member` or `spawn-member`) with `members` members started together:
    /// returns the sum of the members' own rates (an upper bound on the aggregate, every member overlapping)
    /// and the total over the slowest member's time (a lower bound), in units a second.
    fn together(what: &str, members: usize) -> (f64, f64) {
        let me = std::env::current_exe().expect("current exe");
        let mut kids: Vec<Child> = (0..members)
            .map(|_| {
                Command::new(&me)
                    .arg(what)
                    .stdin(Piped::piped())
                    .stdout(Piped::piped())
                    .stderr(Piped::inherit())
                    .spawn()
                    .expect("spawn a member")
            })
            .collect();
        for k in &mut kids {
            k.stdin.take().expect("stdin").write_all(&[1]).expect("go");
        }
        let (mut sum, mut total, mut slowest) = (0.0, 0.0, 0.0f64);
        for k in &mut kids {
            let mut line = String::new();
            BufReader::new(k.stdout.take().expect("stdout"))
                .read_line(&mut line)
                .expect("a member's report");
            let mut words = line.split_whitespace();
            let done: f64 = words.next().expect("done").parse().expect("done");
            let secs = words.next().expect("ns").parse::<f64>().expect("ns") / 1e9;
            sum += done / secs;
            total += done;
            slowest = slowest.max(secs);
            assert!(k.wait().expect("wait").success(), "a member failed");
        }
        (sum, total / slowest)
    }

    fn passes() -> usize {
        std::env::args_os()
            .skip(1)
            .filter_map(|a| a.into_string().ok())
            .filter(|a| a != "--bench")
            .find_map(|a| a.parse().ok())
            .unwrap_or(3)
    }

    pub fn main() {
        let mode = std::env::args_os()
            .skip(1)
            .filter_map(|a| a.into_string().ok())
            .find(|a| {
                ["readcost", "events", "fill", "fill-member", "spawn-member"].contains(&a.as_str())
            });
        if let Some(what @ ("fill-member" | "spawn-member")) = mode.as_deref() {
            member_main(what);
            return;
        }
        let passes = passes();
        match mode.as_deref() {
            Some("readcost") => {
                // The floor, the derived ceilings' range, and the ceiling now.
                let paces_us = [250u64, 1_000, 2_500, 10_000, 25_000, 50_000, 100_000];
                println!("macos_box readcost passes={passes} hold_seconds=2");
                for pass in 1..=passes {
                    for &n in &[1usize, 14, 28] {
                        for &us in &paces_us {
                            let (rate, pct) = read_cost(n, Duration::from_micros(us), 2);
                            println!(
                                "pass={pass} members={n} pace_us={us} reads_per_s={rate:.1} cpu_pct_of_core={pct:.4} load1={:.2}",
                                sys::loadavg().unwrap_or(-1.0)
                            );
                        }
                    }
                }
                return;
            }
            Some("fill") => {
                let cores = std::thread::available_parallelism().map_or(1, |n| n.get());
                println!(
                    "macos_box fill passes={passes} cores={cores} fill_bytes={FILL_BYTES} spawns={SPAWNS}"
                );
                for pass in 1..=passes {
                    for members in [cores, 2 * cores, 4 * cores] {
                        let (sum, span) = together("fill-member", members);
                        println!(
                            "pass={pass} fill members={members} bytes_per_s_sum={sum:.0} bytes_per_s_span={span:.0} load1={:.2}",
                            sys::loadavg().unwrap_or(-1.0)
                        );
                        let (sum, span) = together("spawn-member", members);
                        println!(
                            "pass={pass} spawn members={members} spawns_per_s_sum={sum:.1} spawns_per_s_span={span:.1} load1={:.2}",
                            sys::loadavg().unwrap_or(-1.0)
                        );
                    }
                }
                return;
            }
            Some(_) => {
                println!("macos_box events passes={passes} batch={}", sys::EVENTS);
                for pass in 1..=passes {
                    for &n in &COUNTS {
                        let (first, settled) = events_per_wait(n);
                        println!(
                            "pass={pass} members={n} first_wait_events={first} settled_wait_events={settled} load1={:.2}",
                            sys::loadavg().unwrap_or(-1.0)
                        );
                    }
                }
                return;
            }
            None => {}
        }
        let hex = hex_binary();
        println!("macos_box bound_mib={} passes={passes}", BOUND / MIB);
        for pass in 1..=passes {
            for &n in &COUNTS {
                let over = overshoot(&hex, n);
                println!(
                    "pass={pass} members={n} overshoot_mib={:.1} load1={:.2}",
                    over as f64 / MIB as f64,
                    sys::loadavg().unwrap_or(-1.0),
                );
            }
        }
        for pass in 1..=passes {
            for members in [1u64, 2, 14] {
                for percent in [1u64, 25, 50, 75, 90, 95, 99] {
                    let (pct, held) = watcher_cpu(&hex, 2, percent, members);
                    println!(
                        "pass={pass} members={members} hold_pct_of_bound={percent} hold_seconds=2 watcher_cpu_pct_of_core={pct:.3} held={held} load1={:.2}",
                        sys::loadavg().unwrap_or(-1.0)
                    );
                }
            }
        }
    }
}
