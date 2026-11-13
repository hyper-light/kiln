//! The Windows `process` class against the runtime contract (AC-T7.1.1, Hex HEX-RT-1..9) and the
//! step's own scenarios: a member committing past its bound refused at the bound (the job's peak at or
//! below it), a member asking to break away refused, and delete ending a box's grandchildren. The
//! programs are `hex selftest-box` acts, so the real box is held to the same contract as the simulator
//! in `hex-api` and the other OSes' `process` classes. Runs on Windows (jupiter); the recorded run is
//! the native plan step 3's evidence.
#![cfg(target_os = "windows")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::io::Read;
use std::process::Command;
use std::time::Instant;

use hex_api::conformance::{self, Act, Fixture};
use hex_api::differential;
use hex_api::{Bounds, BoxId, BoxSpec, ClassSpec, Exit, Program, Runtime, Signal, Stdio};
use hex_runtime::windows::WinRuntime;

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;

struct BinFixture {
    exe: String,
}

impl BinFixture {
    fn act(&self, act: &Act) -> Program {
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

    fn named(&self, act: &str) -> Program {
        Program::of(&[&self.exe, "selftest-box", act])
    }
}

impl Fixture for BinFixture {
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
        self.act(act)
    }

    /// Windows has no §3-clean count of a member's own refused fork (the job's accounting doesn't move,
    /// and the only signal is a lossy completion message; docs/research/runtime/windows-pids-refused.md),
    /// so HEX-RT-6 checks the bound's deterministic effect here (the blocked program exits non-zero)
    /// while the class still counts the refusals it sees synchronously (Ada's call, 2026-11-03).
    fn refused_forks_counted(&self) -> bool {
        false
    }

    /// HEX-RT-14: Windows' exec bound, one for both Windows classes (hex_core::hcs::exec_limit).
    fn exec_limit(&self) -> Option<hex_api::ExecLimit> {
        Some(hex_core::hcs::exec_limit())
    }
}

fn runtime() -> WinRuntime {
    WinRuntime::new()
}

fn fixture() -> BinFixture {
    BinFixture {
        exe: env!("CARGO_BIN_EXE_hex").to_owned(),
    }
}

/// The refusal of an `oci` box made from the bundle `config` (a `config.json`), and the runtime, which
/// must hold no box after it.
fn refused_bundle(config: &serde_json::Value) -> hex_api::Refusal {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("config.json"), config.to_string()).unwrap();
    let rt = runtime();
    let refused = rt
        .create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: BoxId::new("oci-on-windows").unwrap(),
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(root.path().to_path_buf()),
            owner: None,
            band: 0,
        })
        .unwrap_err();
    assert!(rt.list().unwrap().is_empty(), "the refused box is left");
    refused
}

/// HEX-INST-2 (native plan step 6): a Windows image's `oci` box needs Windows' Containers feature
/// alone. While it is off the box is refused by name, from the features' state the node read without
/// elevation, naming Containers and saying the installer turns it on; once it is on, no feature is
/// named and the box goes on to the HCS class (native plan step 4), which here refuses its missing
/// layer for what it is. The installer's lab runs this before install and after, and prints which.
#[test]
fn a_windows_image_is_refused_naming_containers_while_it_is_off() {
    let refused = refused_bundle(&serde_json::json!({
        "ociVersion": "1.2.0",
        "process": {"args": ["cmd.exe", "/c", "exit 0"], "cwd": "C:\\"},
        "windows": {"layerFolders": ["C:\\hex-no-such-layer", "C:\\hex-no-such-scratch"]},
    }));
    eprintln!("the oci class, a Windows image: {refused:?}");
    match &refused {
        hex_api::Refusal::Platform {
            what,
            platform,
            why,
        } if why.contains("Hex's installer turns") => {
            assert_eq!((*what, *platform), ("the oci class", "windows"));
            assert!(why.contains("Containers"), "{why}");
            assert!(!why.contains("HypervisorPlatform"), "{why}");
            eprintln!("FEATURES: Containers off");
        }
        other => {
            let text = format!("{other:?}");
            assert!(!text.contains("Hex's installer turns"), "{text}");
            eprintln!("FEATURES: Containers on");
        }
    }
}

/// HEX-INST-2 (native plan steps 5 and 6): a Linux image (a bundle with no `windows` section) runs in
/// a Shards guest, which needs the Windows Hypervisor Platform. It is always refused by name here: while
/// HypervisorPlatform is off, naming it and saying the installer turns it on; once it is on, naming the
/// Shards guest it waits on, and never as a malformed Windows spec.
#[test]
fn a_linux_image_is_refused_naming_what_its_guest_waits_on() {
    let refused = refused_bundle(&serde_json::json!({
        "ociVersion": "1.2.0",
        "process": {"args": ["/bin/true"], "cwd": "/"},
        "root": {"path": "rootfs"},
        "linux": {},
    }));
    eprintln!("the oci class, a Linux image: {refused:?}");
    let hex_api::Refusal::Platform {
        what,
        platform,
        why,
    } = &refused
    else {
        panic!("not refused by name: {refused:?}");
    };
    assert_eq!((*what, *platform), ("the oci class", "windows"));
    assert!(!why.contains("Containers"), "{why}");
    if why.contains("Hex's installer turns") {
        assert!(why.contains("HypervisorPlatform"), "{why}");
        eprintln!("FEATURES: HypervisorPlatform off");
    } else {
        assert!(why.contains("Shards guest"), "{why}");
        eprintln!("FEATURES: HypervisorPlatform on");
    }
}

/// HEX-INST-2 (native plan steps 4, 5 and 6): on the Windows node, whose Linux images are its Shards guest's
/// (`WithGuest::routing` by `WinRuntime::oci_side`), each image goes where it runs, and the node boots its
/// guest before it accepts a box (HEX-GUEST-39). Do: route a Windows image's box and a Linux image's, then
/// make the node on a simulated guest that counts its boots and ends at once. Expect: the Windows image is
/// the host's, and the Linux image the guest's once HypervisorPlatform is on, refused naming it before then.
/// While it is off the node is made with no guest booted, the Windows image stays the host's (step 4's class
/// refuses its missing layer), the Linux image is refused naming it, and no box is left. Once it is on the
/// node boots its guest as it is made, so a guest that ends at once refuses the node after its one boot.
#[test]
fn a_windows_image_stays_the_hosts_and_a_linux_image_goes_to_the_guest() {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use hex_runtime::guest::shares::Share;
    use hex_runtime::guest::{GuestConfig, GuestRuntime, Side, WithGuest};

    let root = tempfile::tempdir().unwrap();
    let bundle = |name: &str, config: serde_json::Value| {
        let dir = root.path().join(name);
        std::fs::create_dir_all(dir.join("rootfs")).unwrap();
        std::fs::write(dir.join("config.json"), config.to_string()).unwrap();
        BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: BoxId::new(name).unwrap(),
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(dir),
            owner: None,
            band: 0,
        }
    };
    let windows = bundle(
        "windows-image",
        serde_json::json!({
            "ociVersion": "1.2.0",
            "process": {"args": ["cmd.exe", "/c", "exit 0"], "cwd": "C:\\"},
            "windows": {"layerFolders": ["C:\\hex-no-such-layer", "C:\\hex-no-such-scratch"]},
        }),
    );
    // A Linux image's root as a tarball: on Windows a directory root is refused by name, NTFS keeping
    // neither its modes nor its links' texts (docs/design/shards-guest.md §4a).
    let linux_root = root.path().join("linux-rootfs.tar");
    tar::Builder::new(std::fs::File::create(&linux_root).unwrap())
        .into_inner()
        .unwrap();
    let linux = bundle(
        "linux-image",
        serde_json::json!({
            "ociVersion": "1.2.0",
            "process": {"args": ["/bin/true"], "cwd": "/"},
            "root": {"path": linux_root.to_str().unwrap()},
            "linux": {},
        }),
    );

    let host = runtime();
    assert_eq!(
        host.oci_side(&windows.class).unwrap(),
        Side::Host,
        "a Windows image left the host"
    );
    let hypervisor = match host.oci_side(&linux.class) {
        Ok(side) => {
            assert_eq!(side, Side::Guest, "a Linux image stayed on the host");
            true
        }
        Err(hex_api::Refusal::Platform { why, .. }) if why.contains("Hex's installer turns") => {
            assert!(why.contains("HypervisorPlatform"), "{why}");
            false
        }
        Err(other) => panic!("a Linux image's side: {other:?}"),
    };
    eprintln!(
        "FEATURES: HypervisorPlatform {}",
        if hypervisor { "on" } else { "off" }
    );

    let boots = Arc::new(AtomicUsize::new(0));
    let booted = Arc::clone(&boots);
    let guests = shards_api::sim::SimGuests::new(move |_host| {
        booted.fetch_add(1, Ordering::SeqCst);
        shards_api::End::Stopped
    });
    let config = GuestConfig {
        // A simulated guest boots nothing real: no binary, kernel or initramfs is read.
        binary: root.path().join("no-shards.exe"),
        kernel: root.path().join("no-kernel"),
        initrd: root.path().join("no-initrd"),
        // The simulated guest reads neither: Shards' own minimums, the smallest guest it boots (Shards
        // a0cced87 crates/vmm/src/vm/runtime.rs:1061 "at least one vCPU is required",
        // crates/vmm/src/vm/x86_64.rs:28 MIN_MEMORY_MIB = 128, an even number of MiB).
        vcpus: 1,
        gpu: false,
        memory_mib: 128,
        shares: vec![Share {
            dir: root.path().to_path_buf(),
            read_only: false,
            at: "/run/hex/shares/0".to_owned(),
        }],
        staged: "/run/hex/staged".to_owned(),
    };
    let made = WithGuest::routing(
        runtime(),
        GuestRuntime::new(Arc::new(guests), config),
        WinRuntime::oci_side,
    );
    if hypervisor {
        let refused = made
            .err()
            .expect("a node whose guest ends at once is refused");
        assert!(
            matches!(&refused, hex_api::Refusal::Os { why, .. } if why.contains("the guest has ended")),
            "{refused:?}"
        );
        assert_eq!(
            boots.load(Ordering::SeqCst),
            1,
            "the node didn't boot its guest as it was made"
        );
        return;
    }
    let node = made.unwrap();
    assert_eq!(
        boots.load(Ordering::SeqCst),
        0,
        "booted with HypervisorPlatform off"
    );
    let windows = node.create(windows).unwrap_err();
    eprintln!("a Windows image: {windows:?}");
    let linux = node.create(linux).unwrap_err();
    assert!(
        matches!(&linux, hex_api::Refusal::Platform { why, .. } if why.contains("HypervisorPlatform")),
        "{linux:?}"
    );
    assert_eq!(boots.load(Ordering::SeqCst), 0, "a create booted the guest");
    assert!(node.list().unwrap().is_empty(), "a refused box is left");
}

/// AC-T7.1.1: the Windows `process` class holds the runtime contract (HEX-RT-1..9; 10..13 are the oci
/// class's, which this one has nothing to check), and the differential suite holds it to the simulator's
/// trace (HEX-DIFF-1..6, CLAUDE.md §8).
#[test]
fn windows_process_boxes_hold_the_contract() {
    let failures = differential::run(&runtime(), &fixture());
    assert!(failures.is_empty(), "conformance failures: {failures:#?}");
}

/// The step's criterion: a member committing past its box's 256 MiB bound is refused at the bound, and
/// the job's peak is at or below the bound. The commit is refused (the member exits non-zero), the box
/// counts reaching its memory bound, and the peak the job reports is at or below 256 MiB.
#[test]
fn a_box_past_its_bound_is_refused_at_the_bound() {
    let rt = runtime();
    let fx = fixture();
    let id = BoxId::new("win-over").unwrap();
    let _ = rt.delete(&id);
    let bound = 256 * MIB;
    rt.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds {
            memory_max: Some(bound),
            ..Bounds::default()
        },
        class: ClassSpec::Process,
        owner: None,
        band: 0,
    })
    .expect("create");
    let mut p = rt
        .exec(&id, fx.act(&Act::Hog(2 * GIB)), Stdio::null())
        .expect("exec hog");
    let exit = end_of(&mut *p, "the hog ends");
    let usage = rt.usage(&id).expect("usage");
    rt.delete(&id).expect("delete");
    assert_ne!(exit, Exit::Code(0), "a hog over the bound exited 0");
    assert!(usage.oom_kills >= 1, "memory bound not counted: {usage:?}");
    assert!(
        usage.memory_peak_bytes <= bound,
        "peak {} over the bound {bound}",
        usage.memory_peak_bytes
    );
}

/// The step's scenario: a box under its bound runs its program to its own exit, reported with its code.
#[test]
fn a_box_under_its_bound_runs_and_reports_its_exit() {
    let rt = runtime();
    let fx = fixture();
    let id = BoxId::new("win-under").unwrap();
    let _ = rt.delete(&id);
    rt.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds {
            memory_max: Some(256 * MIB),
            ..Bounds::default()
        },
        class: ClassSpec::Process,
        owner: None,
        band: 0,
    })
    .expect("create");
    let mut p = rt
        .exec(&id, fx.act(&Act::Fail(7)), Stdio::null())
        .expect("exec");
    let exit = end_of(&mut *p, "the program ends");
    assert_eq!(exit, Exit::Code(7));
    // Once the member has ended and its exit is read, the box is empty (wait on the fact: the ended
    // process leaves the job once its handle is closed).
    wait_until(
        || {
            rt.inspect(&id)
                .map(|i| i.state == hex_api::BoxState::Empty)
                .unwrap_or(false)
        },
        "the box is empty after its program ends",
    );
    rt.delete(&id).expect("delete");
    assert_eq!(rt.inspect(&id), Err(hex_api::Refusal::NoSuchBox(id)));
}

/// A named signal ends the box's job; the member's exit reads as that signal (TERM is 15 on the seam).
#[test]
fn a_box_takes_a_named_signal() {
    let rt = runtime();
    let fx = fixture();
    let id = BoxId::new("win-signal").unwrap();
    let _ = rt.delete(&id);
    rt.create(fx.spec(id.clone(), Bounds::default()))
        .expect("create");
    let mut p = rt
        .exec(&id, fx.act(&Act::Wait), Stdio::null())
        .expect("exec wait");
    wait_until(
        || {
            rt.inspect(&id)
                .map(|i| i.state == hex_api::BoxState::Running)
                .unwrap_or(false)
        },
        "the box runs",
    );
    rt.signal(&id, Signal::TERM).expect("signal TERM");
    let exit = end_of(&mut *p, "the program ends");
    rt.delete(&id).expect("delete");
    assert_eq!(exit, Exit::Signal(15), "TERM is 15 on the seam");
}

/// The step's criterion: a member asking to break away from its box's job is refused. The `win-breakaway`
/// act tries to start a child with `CREATE_BREAKAWAY_FROM_JOB`; the job forbids it, so the start fails
/// and the act exits 0.
#[test]
fn a_member_asking_to_break_away_is_refused() {
    let rt = runtime();
    let fx = fixture();
    let id = BoxId::new("win-breakaway").unwrap();
    let _ = rt.delete(&id);
    rt.create(fx.spec(id.clone(), Bounds::default()))
        .expect("create");
    let (mut r, w) = std::io::pipe().expect("pipe");
    let mut p = rt
        .exec(
            &id,
            fx.named("win-breakaway"),
            Stdio {
                stdout: Some(w),
                ..Stdio::null()
            },
        )
        .expect("exec win-breakaway");
    let exit = end_of(&mut *p, "the act ends");
    let mut out = String::new();
    r.read_to_string(&mut out).expect("read");
    rt.delete(&id).expect("delete");
    assert_eq!(exit, Exit::Code(0), "breakaway was not refused: {out:?}");
    assert!(out.contains("breakaway=refused"), "act said {out:?}");
}

/// A fork the node itself starts past the box's process bound is refused synchronously and counted
/// exactly (unlike a member's own fork, whose refusal only reaches the box asynchronously). A box
/// bounded to 2 holds two waiting members; the node's third exec is refused as Exhausted, and the box
/// counts it in `pids_refused`.
#[test]
fn a_node_started_process_past_the_bound_is_refused_and_counted() {
    let rt = runtime();
    let fx = fixture();
    let id = BoxId::new("win-pids").unwrap();
    let _ = rt.delete(&id);
    rt.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds {
            pids_max: Some(2),
            ..Bounds::default()
        },
        class: ClassSpec::Process,
        owner: None,
        band: 0,
    })
    .expect("create");
    let mut held = Vec::new();
    for _ in 0..2 {
        held.push(
            rt.exec(&id, fx.act(&Act::Wait), Stdio::null())
                .expect("exec within the bound"),
        );
    }
    wait_until(
        || rt.usage(&id).map(|u| u.pids >= 2).unwrap_or(false),
        "the box holds its two members",
    );
    // The third member would exceed the bound: the node's own start is refused. (Match without
    // formatting the Ok, whose Box<dyn Proc> has no Debug.)
    match rt.exec(&id, fx.act(&Act::Wait), Stdio::null()) {
        Err(hex_api::Refusal::Exhausted {
            what: "processes", ..
        }) => {}
        Err(other) => panic!("third exec refused wrongly: {other:?}"),
        Ok(_) => panic!("third exec past the bound was not refused"),
    }
    let usage = rt.usage(&id).expect("usage");
    assert!(
        usage.pids_refused >= 1,
        "node-started refusal not counted: {usage:?}"
    );
    rt.delete(&id).expect("delete");
}

/// The step's criterion: delete ends a box's grandchildren. The `win-grandchild` act (the box's member,
/// a child of the node) starts a grandchild and prints its pid; after delete, that grandchild is gone.
#[test]
fn delete_ends_a_boxs_grandchildren() {
    let rt = runtime();
    let fx = fixture();
    let id = BoxId::new("win-grandchild").unwrap();
    let _ = rt.delete(&id);
    rt.create(fx.spec(id.clone(), Bounds::default()))
        .expect("create");
    let (mut r, w) = std::io::pipe().expect("pipe");
    let mut p = rt
        .exec(
            &id,
            fx.named("win-grandchild"),
            Stdio {
                stdout: Some(w),
                ..Stdio::null()
            },
        )
        .expect("exec win-grandchild");
    // The grandchild's pid, from the member's first line.
    let grandchild = read_grandchild_pid(&mut r);
    // Both the member and its grandchild are in the box now.
    wait_until(
        || rt.usage(&id).map(|u| u.pids >= 2).unwrap_or(false),
        "the box holds the member and its grandchild",
    );
    assert!(
        alive(grandchild),
        "the grandchild {grandchild} isn't running"
    );
    rt.delete(&id).expect("delete");
    let exit = end_of(&mut *p, "the member ends");
    assert!(matches!(exit, Exit::Signal(_)), "member exit {exit:?}");
    // The grandchild is ended too: wait on the fact (its pid gone), not a guessed delay.
    wait_until(|| !alive(grandchild), "the grandchild ends with the box");
}

/// The grandchild's pid from the member's `grandchild=<pid>` line, read from the pipe (blocking until
/// the member writes and flushes it).
fn read_grandchild_pid(r: &mut std::io::PipeReader) -> u32 {
    let deadline = Instant::now() + conformance::patience();
    let mut buf = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        if let Ok(text) = std::str::from_utf8(&buf)
            && let Some(line) = text.lines().find_map(|l| l.strip_prefix("grandchild="))
            && let Ok(pid) = line.trim().parse::<u32>()
        {
            return pid;
        }
        assert!(Instant::now() < deadline, "no grandchild pid: {buf:?}");
        let n = r.read(&mut chunk).expect("read");
        assert!(n > 0, "the member closed its output before a pid: {buf:?}");
        buf.extend_from_slice(&chunk[..n]);
    }
}

/// Whether process `pid` is still running, by asking `tasklist` (pure std, no unsafe in the test): its
/// output names the pid only while it runs.
fn alive(pid: u32) -> bool {
    let out = Command::new("tasklist")
        .args(["/FI", &format!("PID eq {pid}"), "/NH", "/FO", "CSV"])
        .output()
        .expect("tasklist");
    let text = String::from_utf8_lossy(&out.stdout);
    text.contains(&format!("\"{pid}\""))
}

/// `p`'s end, waited for within the suite's learned wait ([`conformance::patience`]), which the end's
/// time teaches (Hex's rule for learned waits); the time is printed, so a run's ends can be recorded.
fn end_of(p: &mut dyn hex_api::Proc, what: &str) -> Exit {
    let asked = Instant::now();
    let exit = p
        .wait_for(conformance::patience())
        .expect("wait")
        .unwrap_or_else(|| panic!("{what}: no end within {:?}", conformance::patience()));
    let took = asked.elapsed();
    conformance::ENDS.saw(took);
    eprintln!("END {what} {} ms", took.as_millis());
    exit
}

fn wait_until(mut cond: impl FnMut() -> bool, what: &str) {
    let deadline = Instant::now() + conformance::patience();
    while Instant::now() < deadline {
        if cond() {
            return;
        }
        std::thread::yield_now();
    }
    panic!("timed out waiting for: {what}");
}
