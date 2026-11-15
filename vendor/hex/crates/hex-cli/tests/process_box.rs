//! The `process` class with real cgroups and real processes: the runtime conformance suite
//! (AC-T7.1.1) and end-to-end crash recovery (T-T7.1.2). The programs are `hex selftest-box` acts,
//! so the real box is held to the same contract as the simulator in `hex-api`.
//!
//! It needs a cgroup v2 subtree the test owns, with cpu, io, memory and pids delegated. Where this
//! process's own cgroup is already private and delegated (a cgroupns-root CI pod), the test uses it
//! directly. Where it is shared (a developer shell, a kiln runner's service cgroup) or lacks one of
//! those controllers (a `kiln run` command's own cgroup), the test re-runs itself once inside a
//! transient `systemd-run --user --scope -p Delegate=yes`, which gives it a private, delegated
//! subtree. Where neither is possible, it returns early with a printed reason rather than a false
//! pass. Run it on a Linux host with cgroup v2 (fedora); the recorded run is CP-T7.1's
//! evidence.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::path::{Path, PathBuf};
use std::process::Command;

use hex_api::conformance::{Act, Fixture};
use hex_api::differential;
use hex_api::{Bounds, BoxId, BoxSpec, ClassSpec, Exit, Program, Refusal, Runtime, Signal, Stdio};
use hex_core::patience::Patience;
use hex_node::Node;
use hex_testkit::waits::follow::{self, Party};

/// The in-process node this test holds, as the party of a wait on what its own threads do (its supervision, its
/// removals): it lives while held, and its threads are its own (the review's C-5). A look reads its state.
fn held_node() -> Party {
    Party::Live
}

struct BinFixture {
    exe: String,
}

impl BinFixture {
    fn wait(&self) -> Program {
        Program::of(&[&self.exe, "selftest-box", "wait"])
    }
}

impl Fixture for BinFixture {
    fn exec_limit(&self) -> Option<hex_api::ExecLimit> {
        Some(hex_runtime::linux::process::exec_limit())
    }
    fn spec(&self, id: BoxId, bounds: Bounds) -> BoxSpec {
        BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id,
            bounds,
            class: ClassSpec::Process,
            owner: None,
            band: 0,
        }
    }

    fn init(&self, id: BoxId, _act: &Act) -> BoxSpec {
        self.spec(id, Bounds::default())
    }

    fn program(&self, act: &Act) -> Program {
        let mut args = vec![self.exe.clone(), "selftest-box".to_owned()];
        match act {
            Act::Say(words) => args.extend(["say".to_owned(), (*words).to_owned()]),
            Act::Fail(code) => args.extend(["fail".to_owned(), code.to_string()]),
            Act::Wait => args.push("wait".to_owned()),
            Act::Hog(bytes) => args.extend(["hog".to_owned(), bytes.to_string()]),
            Act::Spawn(n) => args.extend(["spawn".to_owned(), n.to_string()]),
            Act::Echo => args.push("echo".to_owned()),
        }
        Program::of(&args.iter().map(String::as_str).collect::<Vec<_>>())
    }
}

/// This process's own cgroup, delegated to it and prepared once for every test in this binary
/// (`hex_testkit::cgroup::delegated`: the process and every process of its own moved into a leaf), or why it isn't
/// ours to subdivide. The probe root's opening proves every controller a box uses is delegated here: `BoxRoot::open`
/// refuses by name otherwise (HEX-INST-1).
fn prepare_cgroup(disk: &Path) -> Result<PathBuf, String> {
    hex_testkit::cgroup::delegated(|own| {
        let probe = hex_runtime::linux::cgroup::BoxRoot::open(
            &own.join("hex-probe"),
            hex_runtime::linux::disks::under(disk).unwrap_or_default(),
        )
        .map(|_| ())
        .map_err(|e| e.to_string());
        let _ = std::fs::remove_dir(own.join("hex-probe"));
        probe
    })
}

fn cache_dir() -> PathBuf {
    let cache =
        PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/hex-t7/boxtest");
    std::fs::create_dir_all(&cache).expect("cache dir");
    cache
}

#[test]
fn process_boxes_hold_the_contract_and_recover() {
    let exe = env!("CARGO_BIN_EXE_hex").to_owned();
    let cache = cache_dir();

    match prepare_cgroup(&cache) {
        Ok(own) => {
            exercise_conformance(&own, &cache, &exe);
            exercise_recovery(&own, &cache, &exe);
            exercise_clone_into_cgroup(&own);
            exercise_a_delete_beside_a_runs_removal(&own, &cache, &exe);
            exercise_a_signal_beside_a_runs_removal(&own, &cache, &exe);
            exercise_a_runs_emptying_followed_beside_the_others(&own, &cache, &exe);
        }
        Err(direct) if std::env::var_os("HEX_DELEGATED").is_none() => {
            let has_systemd = Command::new("systemd-run")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !has_systemd {
                hex_testkit::skip(
                    "process_boxes_hold_the_contract_and_recover",
                    "cgroup v2 delegation",
                    &format!(
                        "own cgroup isn't ours to subdivide ({direct}) and no systemd --user for a private scope"
                    ),
                )
                .expect("recording the skip");
                return;
            }
            let test_bin = std::env::current_exe().expect("test binary path");
            let status = Command::new("systemd-run")
                .args(["--user", "--scope", "--quiet", "-p", "Delegate=yes"])
                .arg(&test_bin)
                .args([
                    "process_boxes_hold_the_contract_and_recover",
                    "--exact",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env("HEX_DELEGATED", "1")
                .status()
                .expect("run the test in a delegated scope");
            assert!(status.success(), "the delegated re-run failed");
        }
        // HEX_DELEGATED promised this binary a cgroup delegated to it: a preparation that fails there fails the test,
        // never a skip that would hide the breakage.
        Err(direct) => panic!("its cgroup isn't delegated to it ({direct})"),
    }
}

/// AC-T7.1.1 and decision 7 (docs/design/runtime-architecture.md): a box's program is cloned straight
/// into the box's cgroup, and the cgroup it reads as its own is the box's. A program that isn't there
/// comes back as ENOENT, its child reaped and the box left empty. Where a seccomp profile refuses
/// clone3 (a container's default), the clone hands the program back and the conformance run above
/// has already held the fork path to the contract.
fn exercise_clone_into_cgroup(own: &Path) {
    use std::ffi::CString;
    use std::io::Read;
    use std::os::fd::{AsFd, OwnedFd};

    use hex_api::Exit;
    use hex_runtime::linux::sys::{Cloned, Spawn, clone_into_cgroup};

    let root = hex_runtime::linux::cgroup::BoxRoot::open(&own.join("hex-clone"), Vec::new())
        .expect("a box root for the clone");
    let cg = root
        .create(&BoxId::new("cloned").unwrap(), &Bounds::default())
        .expect("a box");
    let null_in = || OwnedFd::from(std::fs::File::open("/dev/null").unwrap());
    let null_out = || {
        OwnedFd::from(
            std::fs::OpenOptions::new()
                .write(true)
                .open("/dev/null")
                .unwrap(),
        )
    };
    let c = |s: &str| CString::new(s).unwrap();
    // This process's own limit: what this case checks is where the clone lands, not what it starts under.
    let nofile = || rustix::process::getrlimit(rustix::process::Resource::Nofile);
    let (mut out, w) = std::io::pipe().unwrap();
    let spawn = Spawn::new(
        vec![c("/usr/bin/cat")],
        vec![c("cat"), c("/proc/self/cgroup")],
        vec![],
        None,
        [null_in(), OwnedFd::from(w), null_out()],
        nofile(),
    )
    .unwrap();
    let dir = cg.open_dir().unwrap();
    match clone_into_cgroup(dir.as_fd(), spawn).expect("clone3 into the box") {
        Cloned::Started { pid, pidfd } => {
            let mut p = hex_runtime::linux::proc::PidfdProc::of_clone(pid, pidfd);
            assert_eq!(hex_api::conformance::end_of(&mut p), Ok(Exit::Code(0)));
            let mut seen = String::new();
            out.read_to_string(&mut seen).unwrap();
            let rel = cg.dir().strip_prefix("/sys/fs/cgroup").unwrap();
            assert_eq!(
                seen.trim(),
                format!("0::/{}", rel.display()),
                "the cloned program's own cgroup is the box's"
            );
            let missing = Spawn::new(
                vec![c("/nonexistent/hex-test-program")],
                vec![c("hex-test-program")],
                vec![],
                None,
                [null_in(), null_out(), null_out()],
                nofile(),
            )
            .unwrap();
            let e = match clone_into_cgroup(dir.as_fd(), missing) {
                Err(e) => e,
                Ok(_) => panic!("a missing program was started"),
            };
            assert_eq!(e.raw_os_error(), Some(2), "{e}");
            assert!(
                cg.procs().unwrap().is_empty(),
                "the failed clone's child was reaped"
            );
            // The lead's ruling of 2026-11-11 (AC-T7.4.1): the locked-memory limit given a spawn is the one its program
            // starts under, as a VMM that pins its guest's RAM for DMA has its soft limit raised to its hard one
            // (hex-runtime's firecracker vfio): here half the limit this process's programs lock under, in whole KiB, so it
            // can't be the one the program would inherit: its soft RLIMIT_MEMLOCK, or where that is unlimited what the
            // box's cgroups let a program lock ([`lockable`]); with neither finite there is no limit to halve, said.
            let memlock = rustix::process::getrlimit(rustix::process::Resource::Memlock);
            let Some(limit) = memlock.current.or_else(|| lockable(cg.dir())) else {
                hex_testkit::skip(
                    "process_boxes_hold_the_contract_and_recover",
                    "a finite locked-memory limit",
                    "RLIMIT_MEMLOCK is unlimited and every cgroup from the box up has memory.max max",
                )
                .expect("recording the skip");
                cg.remove().unwrap();
                std::fs::remove_dir(root.dir()).unwrap();
                return;
            };
            let soft = limit / 2 / 1024 * 1024;
            let (mut out, w) = std::io::pipe().unwrap();
            let limited = Spawn::new(
                vec![c("/bin/sh")],
                vec![c("sh"), c("-c"), c("ulimit -Sl")],
                vec![],
                None,
                [null_in(), OwnedFd::from(w), null_out()],
                nofile(),
            )
            .unwrap()
            .with_memlock(Some(rustix::process::Rlimit {
                current: Some(soft),
                maximum: memlock.maximum,
            }));
            let Cloned::Started { pid, pidfd } =
                clone_into_cgroup(dir.as_fd(), limited).expect("clone3 into the box")
            else {
                panic!("clone3 served the first program, so it serves this one")
            };
            let mut p = hex_runtime::linux::proc::PidfdProc::of_clone(pid, pidfd);
            assert_eq!(hex_api::conformance::end_of(&mut p), Ok(Exit::Code(0)));
            let mut seen = String::new();
            out.read_to_string(&mut seen).unwrap();
            assert_eq!(
                seen.trim(),
                (soft / 1024).to_string(),
                "the program's soft locked-memory limit, in KiB"
            );
        }
        Cloned::Unsupported(_) => eprintln!(
            "NOTE: clone3 into a cgroup refused here (a seccomp profile or a kernel before 5.7); the fork path served the conformance run"
        ),
    }
    cg.remove().unwrap();
    std::fs::remove_dir(root.dir()).unwrap();
}

/// AC-T7.1.1: the `process` class holds the runtime contract on real cgroups and processes, and the
/// differential suite holds it to the simulator's trace (HEX-DIFF-1..6, CLAUDE.md §8).
fn exercise_conformance(own: &Path, cache: &Path, exe: &str) {
    let state = tempfile::Builder::new()
        .prefix("conf-")
        .tempdir_in(cache)
        .expect("state dir");
    let node =
        Node::open(&own.join("hex-boxes-conf"), state.path(), state.path()).expect("open a node");
    let failures = differential::run(
        &node,
        &BinFixture {
            exe: exe.to_owned(),
        },
    );
    assert!(failures.is_empty(), "conformance failures: {failures:#?}");
}

/// T-T7.1.2: a node agent restart adopts a box still running and scrubs every leftover, leaking none.
fn exercise_recovery(own: &Path, cache: &Path, exe: &str) {
    let state = tempfile::Builder::new()
        .prefix("rec-")
        .tempdir_in(cache)
        .expect("state dir");
    let box_root = own.join("hex-boxes-rec");
    let fx = BinFixture {
        exe: exe.to_owned(),
    };

    // A box with a program still running when the agent "restarts".
    let node = Node::open(&box_root, state.path(), state.path()).expect("node");
    let live = BoxId::new("live").unwrap();
    node.create(fx.spec(live.clone(), Bounds::default()))
        .expect("create live");
    let mut proc = node
        .exec(&live, fx.wait(), Stdio::null())
        .expect("exec wait");
    // Followed by its program's life (decision 9): a program that ended leaves the box never to run.
    follow::until(
        &RUNS,
        "the live box running",
        || match proc.try_wait() {
            Ok(None) => Party::Live,
            Ok(Some(_)) | Err(_) => Party::Ended,
        },
        || {
            node.inspect(&live)
                .map(|i| i.state == hex_api::BoxState::Running)
                .unwrap_or(false)
        },
    )
    .expect("the live box runs");

    // A leftover cgroup with no record (a crash after the cgroup was made, before the record).
    std::fs::create_dir_all(box_root.join("orphan")).expect("orphan cgroup");
    // A leftover record mid-create (a crash after the record, with its cgroup made).
    let midcreate = BoxId::new("midcreate").unwrap();
    let rec = hex_core::record::BoxRecord::new(
        fx.spec(midcreate.clone(), Bounds::default()),
        hex_core::record::Phase::Creating,
        vec![],
    );
    std::fs::write(state.path().join("midcreate.json"), rec.encode().unwrap())
        .expect("midcreate record");
    std::fs::create_dir_all(box_root.join("midcreate")).expect("midcreate cgroup");

    // Two runs' boxes as an agent that stopped left them (decision 7): one whose program still runs,
    // and one whose program ended while no agent was there to remove its box.
    let runs = hex_runtime::linux::cgroup::BoxRoot::open(&box_root, Vec::new()).expect("box root");
    let ran_live = BoxId::new("ran-live").unwrap();
    let live_cg = runs
        .create(&ran_live, &Bounds::default())
        .expect("a run's cgroup");
    let mut cmd = Command::new(exe);
    cmd.args(["selftest-box", "wait"]);
    hex_runtime::linux::process::join_on_exec(&live_cg, &mut cmd).expect("join on exec");
    let mut running = cmd.spawn().expect("a run's program");
    let pid = i32::try_from(running.id()).unwrap();
    let ran_ended = BoxId::new("ran-ended").unwrap();
    runs.create(&ran_ended, &Bounds::default())
        .expect("an ended run's cgroup");
    let mut gone = Command::new("true").spawn().unwrap();
    let gone_pid = gone.id();
    gone.wait().unwrap();
    for (id, program) in [
        (
            &ran_live,
            hex_core::record::ProcId {
                pid: running.id(),
                start_ticks: hex_runtime::linux::oci::state::start_time(pid).expect("start time"),
            },
        ),
        (
            &ran_ended,
            hex_core::record::ProcId {
                pid: gone_pid,
                start_ticks: 1,
            },
        ),
    ] {
        let mut rec = hex_core::record::BoxRecord::new(
            fx.spec(id.clone(), Bounds::default()),
            hex_core::record::Phase::Ready,
            vec![],
        );
        rec.run = Some(program);
        std::fs::write(
            state.path().join(format!("{id}.json")),
            rec.encode().unwrap(),
        )
        .expect("a run's record");
    }

    // The restart: the agent's process ends (`Node::crash`: its store's hold of the state let go, as the kernel lets a
    // dead process's lock go, and none of its code run; one store writes a state directory), and a fresh node on the
    // same directories recovers.
    node.crash();
    let restarted = Node::open(&box_root, state.path(), state.path()).expect("restart");
    let summary = restarted.recover().expect("recover");

    // Decision 7, T-T7.1.2: the run whose program ended is removed, saying so; the one still running is
    // taken back and watched again, so its box goes once its program ends.
    assert!(
        summary
            .removed
            .iter()
            .any(|(id, why)| *id == ran_ended && why.contains("run for had ended")),
        "the ended run's box wasn't removed: {summary:?}"
    );
    assert!(
        !box_root.join("ran-ended").exists(),
        "the ended run's cgroup leaked"
    );
    assert!(
        summary.adopted.contains(&ran_live),
        "the running run's box wasn't taken back: {summary:?}"
    );
    running.kill().expect("end the run's program");
    // Its party is the restarted node, whose supervision removes the box and which lives while this test holds it: the
    // wait follows the fact alone, its run-out past W* told (decision 9).
    follow::until(
        &REMOVED,
        "the taken-back run's box removed once its program ended",
        held_node,
        || restarted.inspect(&ran_live) == Err(hex_api::Refusal::NoSuchBox(ran_live.clone())),
    )
    .expect("the taken-back run's box removed");
    assert!(
        !box_root.join("ran-live").exists(),
        "the taken-back run's cgroup leaked"
    );
    // The node reaped it (it is this process's child) to remove its box; nothing is left to wait for.
    assert!(running.wait().is_err(), "the run's program wasn't reaped");

    assert!(
        summary.adopted.contains(&live),
        "live not adopted: {summary:?}"
    );
    let removed: Vec<&BoxId> = summary.removed.iter().map(|(id, _)| id).collect();
    assert!(
        removed.contains(&&orphan_id()),
        "orphan not removed: {summary:?}"
    );
    assert!(
        removed.contains(&&midcreate),
        "midcreate not removed: {summary:?}"
    );

    // The adopted box is still running; the leftovers are gone, leaking nothing.
    assert_eq!(
        restarted.inspect(&live).unwrap().state,
        hex_api::BoxState::Running
    );
    assert!(!box_root.join("orphan").exists(), "orphan cgroup leaked");
    assert!(
        !box_root.join("midcreate").exists(),
        "midcreate cgroup leaked"
    );
    assert!(
        !state.path().join("midcreate.json").exists(),
        "midcreate record leaked"
    );

    // Clean up: ending the live box ends its program.
    restarted.delete(&live).expect("delete live");
    assert_eq!(
        proc.try_wait().expect("wait"),
        Some(hex_api::Exit::Signal(Signal::KILL.number()))
    );
    assert!(
        restarted.list().unwrap().is_empty(),
        "boxes left after cleanup"
    );
}

/// A claimer's stop at the hold on the runs' removals, and a delete's wait on that removal or its answer beside it:
/// facts no recorded operation makes, each waited for measuring (`hex_testkit::waits`).
static STOPPED: Patience = Patience::measuring("test.hex_cli.process_box.removal_stopped");
/// A live box shown running once its program runs, and a taken-back run's box removed once its program ended: the same.
static RUNS: Patience = Patience::measuring("test.hex_cli.process_box.live_runs");
static REMOVED: Patience = Patience::measuring("test.hex_cli.process_box.taken_back_removed");
static BESIDE: Patience = Patience::measuring("test.hex_cli.process_box.delete_beside_removal");

/// Runtime goal 7 (decision 7), AC-T7.4.1: a delete of a run's box that its program's end is removing answers as that
/// removal does, never beside it. Do: run a waiting program; hold the node's run removals at their start; kill the
/// program through its run, so the supervising thread claims the run and stops before it removes the box, as a
/// claimer descheduled there would; delete the box from another thread; once that delete waits on the removal or
/// answers, let the removal go. Expect: the delete waits rather than answer while the removal is under way, then
/// answers Ok; the run ends Signal(9); and the box is gone.
fn exercise_a_delete_beside_a_runs_removal(own: &Path, cache: &Path, exe: &str) {
    let state = tempfile::Builder::new()
        .prefix("rundel-")
        .tempdir_in(cache)
        .expect("state dir");
    let node =
        Node::open(&own.join("hex-boxes-rundel"), state.path(), state.path()).expect("open a node");
    let fx = BinFixture {
        exe: exe.to_owned(),
    };
    let id = BoxId::new("rundel").unwrap();
    let mut run = node
        .run(
            fx.spec(id.clone(), Bounds::default()),
            fx.wait(),
            Stdio::null(),
        )
        .expect("run a waiting program");
    let held = node.hold_run_removals().expect("hold the runs' removals");
    run.kill().expect("kill the run's program");
    follow::until(
        &STOPPED,
        "the supervising thread's claim of the run, stopped at the hold",
        held_node,
        || held.stopped().unwrap() == 1,
    )
    .unwrap();
    std::thread::scope(|s| {
        let deleting = s.spawn(|| node.delete(&id));
        follow::until(
            &BESIDE,
            "the delete's wait on the removal, or its answer",
            || {
                if deleting.is_finished() {
                    Party::Ended
                } else {
                    Party::Live
                }
            },
            || held.waiting().unwrap() == 1 || deleting.is_finished(),
        )
        .unwrap();
        assert!(
            !deleting.is_finished(),
            "the delete answered beside the run's removal under way"
        );
        drop(held);
        assert_eq!(
            deleting.join().unwrap(),
            Ok(()),
            "the delete once the removal went"
        );
    });
    assert_eq!(
        run.try_wait(),
        Ok(Some(Exit::Signal(Signal::KILL.number())))
    );
    assert_eq!(node.inspect(&id), Err(Refusal::NoSuchBox(id.clone())));
}

/// Runtime goal 7 (decision 7), AC-T7.4.1: a signal to a run's box whose removal is under way is refused typed, never
/// with an OS error. A signal holds none of the box's turns, so the run's claimer can remove the cgroup a signal has
/// found before the signal reads it. Do: run a waiting program; hold the node's run removals at their start; kill the
/// program through its run, so the supervising thread claims the run and stops before it removes the box; take the
/// box's cgroup as a signal finds it, its directory pinned (`Node::signal`'s lookup, `BoxRoot::child`); let the removal
/// go and follow the run to its end, which it reports once the box is gone; then signal TERM and KILL through what was
/// found. Expect: each refused as not running (cgroup v2 removes only an empty cgroup), and the node's own signal of the
/// box, gone, refused as no such box.
fn exercise_a_signal_beside_a_runs_removal(own: &Path, cache: &Path, exe: &str) {
    let state = tempfile::Builder::new()
        .prefix("runsig-")
        .tempdir_in(cache)
        .expect("state dir");
    let root = own.join("hex-boxes-runsig");
    let node = Node::open(&root, state.path(), state.path()).expect("open a node");
    let fx = BinFixture {
        exe: exe.to_owned(),
    };
    let id = BoxId::new("runsig").unwrap();
    let mut run = node
        .run(
            fx.spec(id.clone(), Bounds::default()),
            fx.wait(),
            Stdio::null(),
        )
        .expect("run a waiting program");
    let held = node.hold_run_removals().expect("hold the runs' removals");
    run.kill().expect("kill the run's program");
    follow::until(
        &STOPPED,
        "the supervising thread's claim of the run, stopped at the hold",
        held_node,
        || held.stopped().unwrap() == 1,
    )
    .unwrap();
    let found = hex_runtime::linux::cgroup::BoxCgroup::observe_managed(root.join(id.as_str()))
        .expect("the box's cgroup, looked up")
        .expect("the box's cgroup, still there while its removal is held");
    drop(held);
    assert_eq!(
        hex_api::conformance::end_of(&mut *run),
        Ok(Exit::Signal(Signal::KILL.number())),
        "the run's end, reported once its box is gone"
    );
    for signal in [Signal::TERM, Signal::KILL] {
        assert_eq!(
            found.signal(&id, signal),
            Err(Refusal::NotRunning(id.clone(), "signal")),
            "{signal:?} through the cgroup a signal found before the run's removal took it"
        );
    }
    assert_eq!(
        node.signal(&id, Signal::TERM),
        Err(Refusal::NoSuchBox(id.clone()))
    );
}

/// A run's box past W*, followed (the review's E-3): its program's leaving a process in the box, its emptying's follow,
/// the node's other box's removal beside it, and a delete's wait on its follower: facts no recorded operation makes,
/// each waited for measuring.
static LEFT: Patience = Patience::measuring("test.hex_cli.process_box.program_left_a_process");
static FOLLOWED: Patience = Patience::measuring("test.hex_cli.process_box.emptying_followed");
static REMOVED_BESIDE: Patience =
    Patience::measuring("test.hex_cli.process_box.removed_beside_a_follow");
static WAITS_ON_FOLLOWER: Patience =
    Patience::measuring("test.hex_cli.process_box.delete_waits_on_a_follower");

/// The review's E-3 (decision 9: a wait follows its party by its life, never on a thread other parties share), runtime
/// goal 7, AC-T7.4.1: a run's box that doesn't empty within W* is followed to its emptying by a thread of its own, the
/// supervising thread removing the node's other boxes meanwhile, and a delete of the followed box waits for that
/// thread's word. Do: hold box A's emptying (its cgroup reads as not yet empty, as one whose last process sleeps
/// uninterruptibly in the kernel past W* reads); run A, a shell that leaves a waiting process of its own in the box, and
/// B, a waiting program; once A's box holds both, kill A's shell through its run and leave the run to the supervising
/// thread; once A's emptying is followed, name the thread that follows it; kill B's program the same way; then hold the
/// runs' removals, delete A from another thread, and let A's emptying go once the delete waits. Expect: A followed by a
/// thread of its own, never the supervising thread; B's box removed while A's emptying is held; the delete of A waiting
/// on the follower rather than answering beside it, then Ok; A's box gone.
fn exercise_a_runs_emptying_followed_beside_the_others(own: &Path, cache: &Path, exe: &str) {
    let state = tempfile::Builder::new()
        .prefix("runfol-")
        .tempdir_in(cache)
        .expect("state dir");
    let root = own.join("hex-boxes-runfol");
    let node = Node::open(&root, state.path(), state.path()).expect("open a node");
    let fx = BinFixture {
        exe: exe.to_owned(),
    };
    let a = BoxId::new("runfol-a").unwrap();
    let b = BoxId::new("runfol-b").unwrap();
    let emptying = node.hold_emptying(&a).expect("hold A's emptying");
    let leaves = Program::of(&["/bin/sh", "-c", "\"$0\" selftest-box wait & wait", exe]);
    let mut run_a = node
        .run(fx.spec(a.clone(), Bounds::default()), leaves, Stdio::null())
        .expect("run A's shell");
    let mut run_b = node
        .run(
            fx.spec(b.clone(), Bounds::default()),
            fx.wait(),
            Stdio::null(),
        )
        .expect("run B's waiting program");
    let procs = root.join(a.as_str()).join("cgroup.procs");
    follow::until(
        &LEFT,
        "A's shell and the process it leaves, in A's box",
        || life(&mut *run_a),
        || std::fs::read_to_string(&procs).is_ok_and(|p| p.lines().count() >= 2),
    )
    .unwrap();
    run_a.kill().expect("kill A's shell");
    drop(run_a);
    follow::until(
        &FOLLOWED,
        "A's emptying followed past W*",
        || there(&node, &a),
        || !emptying.following().unwrap().is_empty(),
    )
    .unwrap();
    assert_eq!(
        emptying.following().unwrap(),
        ["hex-run-follow"],
        "the thread following A's emptying: one of its own, never the supervising thread every box shares"
    );
    run_b.kill().expect("kill B's program");
    drop(run_b);
    follow::until(
        &REMOVED_BESIDE,
        "B's box removed while A's emptying is held",
        || {
            if emptying
                .following()
                .unwrap()
                .iter()
                .any(|t| t == "hex-supervisor")
            {
                Party::Ended
            } else {
                Party::Live
            }
        },
        || node.inspect(&b) == Err(Refusal::NoSuchBox(b.clone())),
    )
    .unwrap();
    let removals = node.hold_run_removals().expect("hold the runs' removals");
    std::thread::scope(|s| {
        let deleting = s.spawn(|| node.delete(&a));
        follow::until(
            &WAITS_ON_FOLLOWER,
            "the delete's wait on A's follower, or its answer",
            || there(&node, &a),
            || removals.waiting().unwrap() == 1 || deleting.is_finished(),
        )
        .unwrap();
        assert!(
            !deleting.is_finished(),
            "the delete of A answered beside its follower"
        );
        drop(emptying);
        assert_eq!(
            deleting.join().unwrap(),
            Ok(()),
            "the delete once A's follower took the box apart"
        );
    });
    drop(removals);
    assert_eq!(node.inspect(&a), Err(Refusal::NoSuchBox(a.clone())));
}

/// A run's program as a party: live until it ends.
fn life(proc: &mut dyn hex_api::Proc) -> Party {
    match proc.try_wait() {
        Ok(None) => Party::Live,
        Ok(Some(_)) | Err(_) => Party::Ended,
    }
}

/// Box `id` as a party: live while the node holds it, so what's waited for of it may yet come.
fn there(node: &Node, id: &BoxId) -> Party {
    match node.inspect(id) {
        Ok(_) => Party::Live,
        Err(_) => Party::Ended,
    }
}

/// What a program in the box at `dir` can lock at most where its soft RLIMIT_MEMLOCK is unlimited: the tightest
/// `memory.max` of the box's cgroup and every cgroup above it; none where each is `max`.
fn lockable(dir: &Path) -> Option<u64> {
    dir.ancestors()
        .take_while(|d| d.starts_with("/sys/fs/cgroup"))
        .filter_map(|d| std::fs::read_to_string(d.join("memory.max")).ok())
        .filter_map(|text| text.trim().parse::<u64>().ok())
        .min()
}

fn orphan_id() -> BoxId {
    BoxId::new("orphan").unwrap()
}
