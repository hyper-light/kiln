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
use std::time::{Duration, Instant};

use hex_api::conformance::{Act, Fixture};
use hex_api::differential;
use hex_api::{Bounds, BoxId, BoxSpec, ClassSpec, Program, Runtime, Signal, Stdio};
use hex_node::Node;

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

/// This process's cgroup directory under /sys/fs/cgroup.
fn own_cgroup() -> PathBuf {
    let own = std::fs::read_to_string("/proc/self/cgroup").expect("/proc/self/cgroup");
    let rel = own
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .expect("a 0:: cgroup line")
        .trim_start_matches('/');
    PathBuf::from("/sys/fs/cgroup").join(rel)
}

/// Moves this process into a leaf of its own cgroup and confirms the cgroup can hand every controller
/// down (a box root opens with all of them). Returns the own cgroup dir on success; `Err` when the
/// cgroup isn't ours to subdivide (shared with other processes, or controllers not delegated).
fn prepare_cgroup(disk: &Path) -> Result<PathBuf, String> {
    let own = own_cgroup();
    let leaf = own.join("hex-agent");
    std::fs::create_dir_all(&leaf).map_err(|e| format!("making a leaf for the agent: {e}"))?;
    std::fs::write(leaf.join("cgroup.procs"), format!("{}", std::process::id()))
        .map_err(|e| format!("moving self into a leaf: {e}"))?;
    // The probe root opening proves every controller a box uses is delegated here: `BoxRoot::open`
    // refuses by name otherwise (HEX-INST-1).
    hex_runtime::linux::cgroup::BoxRoot::open(
        &own.join("hex-probe"),
        hex_runtime::linux::disks::under(disk).unwrap_or_default(),
    )
    .map_err(|e| e.to_string())?;
    let _ = std::fs::remove_dir(own.join("hex-probe"));
    Ok(own)
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
        Err(direct) => {
            hex_testkit::skip(
                "process_boxes_hold_the_contract_and_recover",
                "cgroup v2 delegation",
                &format!("none in the delegated scope either ({direct}); run on a cgroup-v2 host"),
            )
            .expect("recording the skip");
        }
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

    use hex_api::{Exit, Proc};
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
            assert_eq!(
                p.wait_for(Duration::from_secs(10)).unwrap(),
                Some(Exit::Code(0))
            );
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
            // (hex-runtime's firecracker vfio): here half this process's soft limit, in whole KiB, so it can't be the
            // one the program would inherit.
            let memlock = rustix::process::getrlimit(rustix::process::Resource::Memlock);
            let soft = memlock.current.unwrap_or(64 << 20) / 2 / 1024 * 1024;
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
            assert_eq!(
                p.wait_for(Duration::from_secs(10)).unwrap(),
                Some(Exit::Code(0))
            );
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
    wait_until(
        || {
            node.inspect(&live)
                .map(|i| i.state == hex_api::BoxState::Running)
                .unwrap_or(false)
        },
        "live box runs",
    );

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

    // The restart: a fresh node on the same directories recovers.
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
    wait_until(
        || restarted.inspect(&ran_live) == Err(hex_api::Refusal::NoSuchBox(ran_live.clone())),
        "the taken-back run's box removed once its program ended",
    );
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

fn orphan_id() -> BoxId {
    BoxId::new("orphan").unwrap()
}

fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::yield_now();
    }
    panic!("timed out waiting for: {what}");
}
