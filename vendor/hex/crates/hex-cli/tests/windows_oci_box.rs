//! The Windows `oci` class against the runtime contract (AC-T7.1.1, Hex HEX-RT-1..13) and the native
//! plan step 4's own criteria: a device not granted isn't visible in a box, a read-only mount refuses
//! writes, and an image the host doesn't run is refused by name before HCS is asked. The boxes run a
//! Windows image process-isolated through HCS; their programs are `hex selftest-box` acts, which every box
//! sees at `C:\hex` (the launcher's folder), so the real box is held to the same contract as the
//! simulator and the other classes.
//!
//! Runs on Windows with the Containers feature on and an image's layer folders named in
//! `HEX_WINDOWS_LAYERS` (topmost first, `;`-separated: a matching nanoserver or servercore image's,
//! e.g. containerd's windows snapshotter folders), and a mismatched image's in
//! `HEX_WINDOWS_MISMATCH_LAYERS` (both written by bench/native/windows/oci_setup.ps1). Without the first
//! it prints SKIP and checks nothing; the step's recorded run (jupiter) sets them, and a SKIP there fails it.
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
use std::path::{Path, PathBuf};

use hex_api::conformance::{self, Act, Fixture};
use hex_api::differential;
use hex_api::{
    Bounds, BoxId, BoxSpec, BoxState, ClassSpec, Exit, Program, Refusal, Runtime, Stdio,
};
use hex_node::recorded::Recorded;
use hex_runtime::windows::{WinRuntime, oci};
use serde_json::json;

/// The USB device interface class (`GUID_DEVINTERFACE_USB_DEVICE`, usbiodef.h): every host has one, and on
/// jupiter a box opens none of them ungranted and all of them granted (the lead's ruling, 2026-11-06). It
/// isn't in Microsoft's table of supported classes (hardware-devices-in-containers.md:45-79), whose four
/// jupiter has none of, and the display class it lists is refused at create by HCS for Docker and Hex alike.
const USB_DEVICE: &str = "A5DCBF10-6530-11D2-901F-00C04FB951ED";

struct Lab {
    /// The folder holding `hex.exe`, mapped into every box as its launcher.
    bin: PathBuf,
    /// The image's read-only layer folders, topmost first.
    layers: Vec<String>,
    /// Bundles and scratch folders, one per box.
    root: tempfile::TempDir,
}

impl Lab {
    fn new() -> Option<Self> {
        let layers: Vec<String> = std::env::var("HEX_WINDOWS_LAYERS")
            .ok()?
            .split(';')
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect();
        if layers.is_empty() {
            return None;
        }
        let exe = PathBuf::from(env!("CARGO_BIN_EXE_hex"));
        Some(Self {
            bin: exe.parent().unwrap().to_path_buf(),
            layers,
            root: tempfile::tempdir().unwrap(),
        })
    }

    fn runtime(&self) -> WinRuntime {
        WinRuntime::new().with_launcher(self.bin.clone())
    }

    /// `hex selftest-box <act>` as a box sees it.
    fn args(act: &Act) -> Vec<String> {
        let mut args = vec![r"C:\hex\hex.exe".to_owned(), "selftest-box".to_owned()];
        match act {
            Act::Say(words) => args.extend(["say".to_owned(), (*words).to_owned()]),
            Act::Fail(code) => args.extend(["fail".to_owned(), code.to_string()]),
            Act::Wait => args.push("wait".to_owned()),
            Act::Hog(bytes) => args.extend(["hog".to_owned(), bytes.to_string()]),
            Act::Spawn(n) => args.extend(["spawn".to_owned(), n.to_string()]),
            Act::Echo => args.push("echo".to_owned()),
        }
        args
    }

    /// A bundle for box `id` whose init runs `args`, with `mounts` and the spec's `windows` extras.
    fn bundle(&self, id: &BoxId, args: &[String], mounts: serde_json::Value) -> PathBuf {
        let dir = self.root.path().join(format!("bundle-{id}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let scratch = self.root.path().join(format!("scratch-{id}"));
        let mut folders = self.layers.clone();
        folders.push(scratch.to_str().unwrap().to_owned());
        let config = json!({
            "ociVersion": "1.2.0",
            "process": {"args": args, "cwd": "C:\\"},
            "mounts": mounts,
            "windows": {"layerFolders": folders},
        });
        std::fs::write(dir.join("config.json"), config.to_string()).unwrap();
        dir
    }

    fn spec_with(
        &self,
        id: BoxId,
        bounds: Bounds,
        act: &Act,
        mounts: serde_json::Value,
    ) -> BoxSpec {
        let dir = self.bundle(&id, &Self::args(act), mounts);
        BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id,
            bounds,
            class: ClassSpec::Bundle(dir),
            owner: None,
        }
    }
}

impl Fixture for Lab {
    fn spec(&self, id: BoxId, bounds: Bounds) -> BoxSpec {
        self.spec_with(id, bounds, &Act::Wait, json!([]))
    }

    fn init(&self, id: BoxId, act: &Act) -> BoxSpec {
        self.spec_with(id, Bounds::default(), act, json!([]))
    }

    fn program(&self, act: &Act) -> Program {
        let args = Self::args(act);
        Program::of(&args.iter().map(String::as_str).collect::<Vec<_>>())
    }

    /// A member's own refused fork has no §3-clean count on Windows; the bound's effect is checked
    /// (docs/research/runtime/windows-pids-refused.md; Ada's call, 2026-11-03), as for the process class.
    fn refused_forks_counted(&self) -> bool {
        false
    }

    /// HEX-RT-14: Windows' exec bound, the class's own (hex_core::hcs::exec_limit).
    fn exec_limit(&self) -> Option<hex_api::ExecLimit> {
        Some(hex_core::hcs::exec_limit())
    }

    /// TERM ends the box, init included: Windows has no POSIX signals. HCS's ProcessSignal carries only
    /// console controls (Virtualization-Documentation hcs/SchemaReference.md:796-808), hcsshim maps TERM to
    /// a shutdown and KILL to terminate (internal/signals/signal.go:185-197), Docker ends the container on
    /// either (libcontainerd/local/local_windows.go:713-750), and the OCI spec's Windows section defines no
    /// signals (runtime-spec runtime.md:131-136 only asks that kill send one). The class ends its job
    /// (docs/research/runtime/windows-hcs.md §4; HEX-DIFF-1, 2; the lead's ruling, 2026-11-05).
    fn term_ends_init(&self) -> bool {
        true
    }
}

/// Runs `hex selftest-box <args>` in started box `id` and returns its exit and stdout.
fn run(rt: &dyn Runtime, lab: &Lab, id: &BoxId, args: &[&str]) -> (Exit, String) {
    let (mut r, w) = std::io::pipe().unwrap();
    let mut program = lab.program(&Act::Wait);
    program.args = [r"C:\hex\hex.exe", "selftest-box"]
        .iter()
        .chain(args)
        .map(|a| (*a).to_owned())
        .collect();
    let mut p = rt
        .exec(
            id,
            program,
            Stdio {
                stdout: Some(w),
                ..Stdio::null()
            },
        )
        .expect("exec");
    let exit = p
        .wait_for(conformance::PATIENCE)
        .expect("wait")
        .expect("the program ends");
    let mut out = String::new();
    r.read_to_string(&mut out).unwrap();
    (exit, out)
}

/// T-T7.1.2: the end of an init held by its native process handle, using the same measured program-end
/// bound as a Proc's wait_for. The held process handle keeps the init's identity stable through the wait.
fn exit_of(process: &hex_runtime::windows::sys::Handle, bound: std::time::Duration) {
    let millis = u32::try_from(bound.as_millis()).unwrap();
    assert!(
        hex_runtime::windows::sys::wait(process, millis).expect("waiting for the init"),
        "the box's init still runs {bound:?} after it was ended"
    );
}

fn made(rt: &WinRuntime, spec: BoxSpec) -> BoxId {
    let id = spec.id.clone();
    let _ = rt.delete(&id);
    rt.create(spec).expect("create");
    rt.start(&id).expect("start");
    id
}

/// AC-T7.1.1: the Windows `oci` class holds the runtime contract, HEX-RT-1..13, and the differential
/// suite holds it to the simulator's trace for a bundle-made box (HEX-DIFF-1..6, CLAUDE.md §8).
fn contract(lab: &Lab) {
    let failures = differential::run(&lab.runtime(), lab);
    assert!(failures.is_empty(), "conformance failures: {failures:#?}");
}

/// The step's criterion: "a device not granted isn't visible", read as reachable (the lead's ruling,
/// 2026-11-06: a process-isolated silo lists the host's devices under every runtime, and a grant plumbs
/// them in). Do: on a host with USB devices, probe that class from a box granted nothing, then from one
/// granted it. Expect: ungranted, the host's devices are listed and none opens; granted, they open.
fn a_device_not_granted_isnt_visible(lab: &Lab) {
    let host = hex_runtime::windows::oci::devices(USB_DEVICE).expect("the host's devices");
    assert!(
        host.reachable >= 1,
        "the host opens no USB device, so a box's view can't be checked against it: {host:?}"
    );
    let rt = lab.runtime();
    let mut seen = Vec::new();
    for granted in [false, true] {
        let id = BoxId::new(&format!("hex-oci-devices-{granted}")).unwrap();
        let mut spec = lab.spec(id.clone(), Bounds::default());
        if granted {
            let ClassSpec::Bundle(dir) = &spec.class else {
                panic!("the lab makes bundle boxes")
            };
            let path = dir.join("config.json");
            let mut cfg: serde_json::Value =
                serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            cfg["windows"]["devices"] = json!([{"id": USB_DEVICE, "idType": "class"}]);
            std::fs::write(&path, cfg.to_string()).unwrap();
            spec = BoxSpec {
                class: ClassSpec::Bundle(dir.clone()),
                ..spec
            };
        }
        let id = made(&rt, spec);
        let (exit, out) = run(&rt, lab, &id, &["win-devices", USB_DEVICE]);
        rt.delete(&id).expect("delete");
        assert_eq!(exit, Exit::Code(0), "the probe failed: {out:?}");
        seen.push(out.trim().to_owned());
    }
    assert_eq!(
        seen[0],
        format!("{} 0", host.listed),
        "ungranted: the host's USB devices are listed and none opens"
    );
    assert_eq!(
        seen[1],
        format!("{} {}", host.listed, host.reachable),
        "granted: the box opens the host's USB devices"
    );
}

/// The step's criterion: a read-only mount refuses writes. Do: map one host folder read-only and another
/// read-write, write a file into each from inside the box. Expect: the read-only write is refused and
/// leaves nothing on the host; the read-write one lands.
fn a_read_only_mount_refuses_writes(lab: &Lab) {
    let ro = lab.root.path().join("ro");
    let rw = lab.root.path().join("rw");
    std::fs::create_dir_all(&ro).unwrap();
    std::fs::create_dir_all(&rw).unwrap();
    let rt = lab.runtime();
    let id = made(
        &rt,
        lab.spec_with(
            BoxId::new("hex-oci-mounts").unwrap(),
            Bounds::default(),
            &Act::Wait,
            json!([
                {"destination": "C:\\ro", "source": ro.to_str().unwrap(), "options": ["ro"]},
                {"destination": "C:\\rw", "source": rw.to_str().unwrap()},
            ]),
        ),
    );
    let refused = run(&rt, lab, &id, &["write", r"C:\ro\f"]);
    let wrote = run(&rt, lab, &id, &["write", r"C:\rw\f"]);
    rt.delete(&id).expect("delete");
    assert_ne!(
        refused.0,
        Exit::Code(0),
        "a write to a read-only mount: {refused:?}"
    );
    assert!(refused.1.starts_with("refused"), "{refused:?}");
    assert!(!ro.join("f").exists(), "the refused write reached the host");
    assert_eq!(
        wrote.0,
        Exit::Code(0),
        "a write to a read-write mount: {wrote:?}"
    );
    assert!(
        rw.join("f").exists(),
        "the read-write write didn't reach the host"
    );
}

/// The step's criterion: an image whose version doesn't match the host's is refused by name, before HCS
/// is asked. Do: make a box from the layers in `HEX_WINDOWS_MISMATCH_LAYERS`, an image Microsoft's table
/// says this host doesn't run (oci_setup.ps1 pulls Server 2019's nanoserver; a client's own `ntdll.dll`
/// is no mismatch, since Windows 11 25H2's is build 26100, Server 2025's). Expect: create refused as the
/// image, naming the image's build and the host's, and no box made.
fn a_mismatched_image_is_refused_by_name(lab: &Lab) {
    let layers: Vec<String> = std::env::var("HEX_WINDOWS_MISMATCH_LAYERS")
        .expect("HEX_WINDOWS_MISMATCH_LAYERS names a mismatched image's layers (oci_setup.ps1)")
        .split(';')
        .filter(|l| !l.is_empty())
        .map(str::to_owned)
        .collect();
    let base = layers.last().expect("a mismatched image's layers");
    let image = hex_runtime::windows::oci::sys::file_build(&format!(
        r"{base}\Files\Windows\System32\ntdll.dll"
    ))
    .unwrap();
    let host = hex_runtime::windows::oci::sys::host();
    // Both halves of the table on this host: the lab's own image is accepted (on jupiter, Windows 11 25H2
    // at 26200 runs nanoserver:ltsc2025 at 26100, the half a host-ntdll premise got wrong), the other
    // refused.
    let own_base = lab.layers.last().expect("the lab's image layers");
    let own = hex_runtime::windows::oci::sys::file_build(&format!(
        r"{own_base}\Files\Windows\System32\ntdll.dll"
    ))
    .unwrap();
    assert!(
        hex_core::hcs::image_runs(host, own).is_ok(),
        "the lab's image (build {own}) is refused on this host ({host:?})"
    );
    assert!(
        hex_core::hcs::image_runs(host, image).is_err(),
        "the mismatched image (build {image}) runs on this host ({host:?}): pick another"
    );
    let lab_mismatch = Lab {
        bin: lab.bin.clone(),
        layers,
        root: tempfile::tempdir().unwrap(),
    };
    let rt = lab.runtime();
    let id = BoxId::new("hex-oci-mismatch").unwrap();
    match rt.create(lab_mismatch.spec(id.clone(), Bounds::default())) {
        Err(hex_api::Refusal::Platform { what, why, .. }) => {
            assert_eq!(what, "the oci class's image");
            assert!(why.contains(&image.to_string()), "{why}");
            assert!(why.contains(&host.build.to_string()), "{why}");
        }
        other => panic!("a mismatched image (build {image}): {other:?}"),
    }
    assert!(rt.inspect(&id).is_err(), "a refused create made a box");
}

/// The volumes attached on this host (`mountvol`'s `\\?\Volume{..}` lines).
fn volumes() -> Vec<String> {
    let out = std::process::Command::new("mountvol").output().unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with(r"\\?\Volume{"))
        .map(str::to_owned)
        .collect()
}

/// Whether HCS holds a compute system named `id` (`hcsdiag list`).
fn hcs_holds(id: &BoxId) -> bool {
    let out = std::process::Command::new("hcsdiag")
        .arg("list")
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout).contains(id.as_str())
}

/// Native step 4's teardown (the lead's condition, 2026-11-06): do: start a box, write in it, delete it.
/// Expect: nothing left — no compute system, the box's own volume gone, no scratch folder;
/// `UnprepareLayer`'s ERROR_FLT_DO_NOT_DETACH is matched alone, so any other failure fails the delete.
fn a_deleted_box_leaves_nothing(lab: &Lab) {
    let rt = lab.runtime();
    let id = made(
        &rt,
        lab.spec(BoxId::new("hex-oci-teardown").unwrap(), Bounds::default()),
    );
    let wrote = run(&rt, lab, &id, &["write", r"C:\scratch-was-here"]);
    assert_eq!(wrote.0, Exit::Code(0), "{wrote:?}");
    // The box's own volume, by its scratch folder, not a count of the host's: other work on the host makes
    // and removes volumes too (jupiter, 2026-11-08: criterion 2's bench beside this suite).
    let folder = lab
        .root
        .path()
        .join(format!("scratch-{id}"))
        .to_str()
        .unwrap()
        .to_owned();
    let own =
        oci::sys::layer_mount_path(&folder).expect("the started box shows no volume of its own");
    let listed = format!("{own}\\");
    assert!(
        volumes().contains(&listed),
        "the box's volume {own} isn't mounted"
    );
    rt.delete(&id).expect("delete");
    assert!(!hcs_holds(&id), "HCS still holds {id}");
    assert!(
        oci::sys::layer_mount_path(&folder).is_err(),
        "the box's scratch is still activated"
    );
    assert!(
        !volumes().contains(&listed),
        "the box's volume {own} outlived it"
    );
    assert!(
        !std::path::Path::new(&folder).exists(),
        "the scratch folder outlived its box"
    );
}

/// Native step 4 (the lead's condition, 2026-11-06): copies of one `blank.vhdx` share a volume identity, so
/// do: create and start `n` boxes at once, each writing a file of its own. Expect: every one starts, and
/// none sees another's file — each runs on a volume of its own. `n` is 8 and 32, the plan's concurrency
/// points (its throughput is measured at 1, 8 and 32: bench/competitive/README.md:24; 1 is every other case).
fn boxes_started_at_once_each_have_their_own_volume(lab: &Lab, n: usize) {
    let rt = lab.runtime();
    let ids: Vec<BoxId> = (0..n)
        .map(|i| BoxId::new(&format!("hex-oci-at-once-{n}-{i}")).unwrap())
        .collect();
    let specs: Vec<BoxSpec> = ids
        .iter()
        .map(|id| lab.spec(id.clone(), Bounds::default()))
        .collect();
    std::thread::scope(|s| {
        let started: Vec<_> = specs
            .into_iter()
            .map(|spec| {
                let rt = &rt;
                s.spawn(move || {
                    let id = spec.id.clone();
                    let _ = rt.delete(&id);
                    rt.create(spec).and_then(|_| rt.start(&id))
                })
            })
            .collect();
        for (id, h) in ids.iter().zip(started) {
            let got = h.join().unwrap();
            assert!(got.is_ok(), "box {id} of {n} started at once: {got:?}");
        }
    });
    for (i, id) in ids.iter().enumerate() {
        let wrote = run(&rt, lab, id, &["write", &format!(r"C:\mark-{i}")]);
        assert_eq!(wrote.0, Exit::Code(0), "{id}: {wrote:?}");
    }
    for (i, id) in ids.iter().enumerate() {
        let other = (i + 1) % n;
        let (exit, seen) = run(&rt, lab, id, &["exists", &format!(r"C:\mark-{other}")]);
        assert_eq!(exit, Exit::Code(0));
        assert_eq!(
            seen.trim(),
            "no",
            "{id} sees box {other}'s file: they share a volume"
        );
    }
    for id in &ids {
        rt.delete(id).expect("delete");
    }
}

/// The lab's tests one at a time, as the ordered suite runs its cases: one runtime's boxes at a time, so
/// the cases don't contend for one container image's scratch copies, and each can still run alone.
static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn one_at_a_time() -> std::sync::MutexGuard<'static, ()> {
    ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// AC-T7.1.1 and native step 4: the Windows oci class end to end on this host's image, in order (one
/// runtime's boxes at a time, so the cases don't contend for one container image's scratch copies).
#[test]
fn windows_oci_boxes_hold_the_contract_and_the_steps_criteria() {
    let Some(lab) = Lab::new() else {
        eprintln!("SKIP: HEX_WINDOWS_LAYERS names no image layers");
        return;
    };
    let _one = one_at_a_time();
    eprintln!("RUN: layers {:?}", lab.layers);
    a_mismatched_image_is_refused_by_name(&lab);
    a_deleted_box_leaves_nothing(&lab);
    a_killed_nodes_scratch_is_reclaimed_at_the_next_start(&lab);
    boxes_started_at_once_each_have_their_own_volume(&lab, 8);
    boxes_started_at_once_each_have_their_own_volume(&lab, 32);
    contract(&lab);
    a_read_only_mount_refuses_writes(&lab);
    a_device_not_granted_isnt_visible(&lab);
    eprintln!("PASS: HEX-RT-1..13, device, read-only mount, version refusal");
}

/// A box's scratch path never escapes the lab's temp root (a sanity check on the fixture itself).
#[test]
fn the_fixture_keeps_each_boxs_files_under_its_root() {
    let lab = Lab {
        bin: PathBuf::from(r"C:\x"),
        layers: vec![r"D:\l".to_owned()],
        root: tempfile::tempdir().unwrap(),
    };
    let dir = lab.bundle(&BoxId::new("b").unwrap(), &["a".to_owned()], json!([]));
    assert!(dir.starts_with(lab.root.path()));
    assert!(dir.join("config.json").exists());
}

/// The bundle a crash test's child node makes, at a path the parent knows: `root\bundles\<id>`, its
/// scratch `root\bundles\<id>-scratch` (a bundle's scratch is the caller's folder, last in layerFolders).
fn crash_bundle(root: &std::path::Path, id: &str, layers: &[String]) -> (PathBuf, PathBuf) {
    let dir = root.join("bundles").join(id);
    let scratch = root.join("bundles").join(format!("{id}-scratch"));
    std::fs::create_dir_all(&dir).unwrap();
    let mut folders = layers.to_vec();
    folders.push(scratch.to_str().unwrap().to_owned());
    let config = json!({
        "ociVersion": "1.2.0",
        "process": {"args": [r"C:\hex\hex.exe", "selftest-box", "wait"], "cwd": "C:\\"},
        "windows": {"layerFolders": folders},
    });
    std::fs::write(dir.join("config.json"), config.to_string()).unwrap();
    (dir, scratch)
}

/// The child half of the crash test: when HEX_CRASH_NODE is `<phase>;<root>`, act as a node that
/// creates (phase `created`) or creates and starts (phase `started`) one box under `root`, say READY, and
/// wait to be killed; or (phase `reclaim`) as the next node, which reclaims what a killed one left under
/// `root` and says RECLAIMED with its failures. Without it, nothing.
#[test]
fn crash_child_node() {
    let Ok(arg) = std::env::var("HEX_CRASH_NODE") else {
        return;
    };
    let (phase, root) = arg.split_once(';').unwrap();
    let root = PathBuf::from(root);
    if phase == "reclaim" {
        let rt = WinRuntime::with(4, root.join("scratch"));
        println!("RECLAIMED {:?}", rt.reclaim_failures());
        return;
    }
    let lab = Lab::new().expect("the crash test runs with HEX_WINDOWS_LAYERS");
    let rt = WinRuntime::with(4, root.join("scratch")).with_launcher(lab.bin.clone());
    let id = BoxId::new(&format!("hex-crash-{phase}")).unwrap();
    let (dir, _) = crash_bundle(&root, id.as_str(), &lab.layers);
    rt.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Bundle(dir),
        owner: None,
    })
    .expect("create");
    if phase == "started" {
        rt.start(&id).expect("start");
    }
    println!("READY");
    loop {
        std::thread::park();
    }
}

/// The slowest end on record of a killed node's compute system, from the kill to HCS no longer listing it:
/// 2,000 ms (bench/native/records/hcs-end-under-load-8d8c36c-20261108T0036Z.txt; 780 ms the slowest of 15
/// under criterion 2's load, the idle host's one sample read at its 1 s resolution's upper bound).
const HCS_END_RECORD: std::time::Duration = std::time::Duration::from_millis(2_000);

/// The crash test's wait for HCS to end a killed node's system: Hex's learned wait, seeded from
/// [`HCS_END_RECORD`] (the lead, 2026-11-08).
static HCS_ENDS: conformance::LearnedWait = conformance::LearnedWait::new(HCS_END_RECORD);

/// Native step 4 (the lead, 2026-11-06): a node killed with boxes made leaves no scratch behind once a
/// node starts again on the same scratch root. Do: run a child node that creates a box (and, once, starts
/// it), kill it, and start a new runtime on its scratch root. Expect: no sandbox.vhdx copy, no activated
/// volume (vmcompute names no mount path for the scratch), no scratch marker, and HCS ending the box's
/// system without being asked. The container ends with the node by design (ShouldTerminateOnLastHandleClosed, kill-on-close);
/// the scratch is what a crash leaves (CLAUDE.md §2).
fn a_killed_nodes_scratch_is_reclaimed_at_the_next_start(lab: &Lab) {
    let exe = std::env::current_exe().unwrap();
    for phase in ["created", "started"] {
        let root = lab.root.path().join(format!("crash-{phase}"));
        std::fs::create_dir_all(&root).unwrap();
        let mut child = std::process::Command::new(&exe)
            .args([
                "--exact",
                "crash_child_node",
                "--nocapture",
                "--test-threads=1",
            ])
            .env("HEX_CRASH_NODE", format!("{phase};{}", root.display()))
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        let mut out = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            let n = std::io::BufRead::read_line(&mut out, &mut line).unwrap();
            assert!(n > 0, "the child node ended before READY");
            // The harness writes `test crash_child_node ... ` with no newline before it, so READY ends
            // the line rather than filling it.
            if line.trim_end().ends_with("READY") {
                break;
            }
        }
        child.kill().unwrap();
        child.wait().unwrap();
        let id = format!("hex-crash-{phase}");
        // HCS ends a system whose last handle closed on its own time, not before kill returns (jupiter,
        // 2026-11-07 15:24Z: still listed when the next runtime had reclaimed). So the test waits on that
        // fact, the box ending with its node, before the next node starts; HCS_ENDS bounds the wait and is
        // not what passes it.
        let began = std::time::Instant::now();
        while hcs_holds(&BoxId::new(&id).unwrap()) {
            assert!(
                began.elapsed() < HCS_ENDS.bound(),
                "{phase}: HCS still holds {id} {:?} after its node was killed",
                HCS_ENDS.bound()
            );
        }
        HCS_ENDS.saw(began.elapsed());
        let scratch = root.join("bundles").join(format!("{id}-scratch"));
        assert!(
            scratch.join("sandbox.vhdx").exists(),
            "{phase}: the killed node left no scratch to reclaim"
        );
        // A node starting on the same scratch root reclaims what the killed one made.
        let rt = WinRuntime::with(4, root.join("scratch")).with_launcher(lab.bin.clone());
        assert!(
            rt.reclaim_failures().is_empty(),
            "{phase}: {:?}",
            rt.reclaim_failures()
        );
        assert!(
            !scratch.join("sandbox.vhdx").exists(),
            "{phase}: the scratch copy outlived its node"
        );
        assert!(
            oci::sys::layer_mount_path(scratch.to_str().unwrap()).is_err(),
            "{phase}: the scratch volume is still activated"
        );
        let markers: Vec<_> = std::fs::read_dir(root.join("scratch"))
            .map(|d| d.filter_map(Result::ok).map(|e| e.path()).collect())
            .unwrap_or_default();
        assert!(markers.is_empty(), "{phase}: markers left {markers:?}");
    }
}

/// Asserts HCS holds box `id`'s container all through HCS_ENDS' bound (the crash test's learned wait) after
/// its node's handles closed: HCS ends a container made to end with its last handle on its own time, within
/// that bound, so one that outlives its node is still held all that while.
fn outlived(id: &BoxId) {
    let dropped = std::time::Instant::now();
    while dropped.elapsed() < HCS_ENDS.bound() {
        assert!(
            oci::sys::open_system(id.as_str())
                .expect("asking HCS for the box's container")
                .is_some(),
            "HCS ended {id}'s container with its node's last handle, {:?} after",
            dropped.elapsed()
        );
    }
}

/// What a test of an outliving box leaves, ended however the test ends: its container, by id, and its
/// record and scratch, by the next node's reclaim, so no scratch stays attached on the host (jupiter's
/// NTFS stall of 2026-11-08 most likely followed a scratch left attached by a crash test's red run).
struct Leftover<'a> {
    id: &'a BoxId,
    markers: &'a std::path::Path,
}

impl Leftover<'_> {
    /// Ends the box's container through a handle reopened by its id, as its next node would.
    fn end_container(&self) {
        if let Ok(Some(system)) = oci::sys::open_system(self.id.as_str()) {
            let _ = system.terminate(oci::launch::PATIENCE);
        }
    }
}

impl Drop for Leftover<'_> {
    fn drop(&mut self) {
        self.end_container();
        let _ = oci::reclaim(self.markers);
    }
}

/// T-T7.1.2, native plan step 8: do: make an `oci` box whose container outlives its node, drop it without
/// a delete, as a node going down does, and run the next node's reclaim on its markers; then end its
/// container by its id and reclaim again. Expect: HCS still holds the container after the drop, and the
/// reclaim leaves the box's record and its scratch (the sandbox attached) while it does; once the container
/// is ended through the reopened handle, the next reclaim leaves no record, no scratch and nothing
/// attached.
#[test]
fn an_outliving_box_s_container_and_scratch_wait_for_its_next_node() {
    let Some(lab) = Lab::new() else {
        eprintln!("SKIP: HEX_WINDOWS_LAYERS names no image layers");
        return;
    };
    let _one = one_at_a_time();
    let id = BoxId::new("hex-oci-outlives").unwrap();
    let markers = lab.root.path().join("markers");
    let marker = markers.join(format!("{id}.scratch.json"));
    let folder = lab.root.path().join(format!("scratch-{id}"));
    let scratch = folder.to_str().unwrap().to_owned();
    let left = Leftover {
        id: &id,
        markers: &markers,
    };
    let dir = lab.bundle(&id, &Lab::args(&Act::Wait), json!([]));
    let container = hex_core::hcs::from_bundle(&oci::bundle_config(&dir).unwrap()).unwrap();
    // Default bounds set no CPU cap, so the processor count isn't read.
    let job = hex_runtime::windows::process::JobBox::create(
        id.clone(),
        &Bounds::default(),
        std::num::NonZeroU32::MIN,
    )
    .expect("the box's job");
    let made = oci::OciBox::create(
        job,
        &container,
        &lab.bin,
        Some(marker.clone()),
        hex_core::hcs::Lifetime::Outlives,
    )
    .expect("create");
    // Its node goes down: the box is dropped, not deleted.
    drop(made);
    outlived(&id);
    let failures = oci::reclaim(&markers);
    assert!(failures.is_empty(), "{failures:?}");
    assert!(
        marker.exists(),
        "the next node's reclaim took the record of a box whose container runs"
    );
    assert!(
        folder.join("sandbox.vhdx").exists(),
        "the next node's reclaim removed the scratch of a box whose container runs"
    );
    assert!(
        oci::sys::layer_mount_path(&scratch).is_ok(),
        "the scratch was detached from under its running container"
    );
    // Its container ended by id, the next reclaim removes what is left.
    left.end_container();
    let failures = oci::reclaim(&markers);
    assert!(failures.is_empty(), "{failures:?}");
    assert!(!marker.exists(), "the record outlived its container");
    assert!(
        oci::sys::layer_mount_path(&scratch).is_err(),
        "the scratch is still attached"
    );
    assert!(
        !folder.exists(),
        "the scratch folder outlived its container"
    );
}

/// The keeper for a node's runtime in these tests: this test binary run again for [`keeper_body`] alone,
/// the real keeper (`keeper::run`). The runtime hands it its state folder and ready path through the
/// environment, as it hands `hex keep` them, so the command is only how to launch it.
fn keeper() -> Vec<String> {
    vec![
        std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned(),
        "--exact".to_owned(),
        "keeper_body".to_owned(),
        "--ignored".to_owned(),
        "--nocapture".to_owned(),
        "--test-threads".to_owned(),
        "1".to_owned(),
    ]
}

/// The keeper, run as a process of its own by [`keeper`]. `#[ignore]`d, so the harness runs it only when
/// named.
#[test]
#[ignore]
fn keeper_body() {
    let args =
        hex_runtime::windows::keeper::Args::parse(&[]).expect("keeper args from the environment");
    hex_runtime::windows::keeper::run(&args).expect("the keeper runs");
}

/// A node's keeper, ended as a crash would end it (taskkill /F) however the test ends, so no box's job is
/// held past the test.
struct Kept(u32);

impl Drop for Kept {
    fn drop(&mut self) {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &self.0.to_string(), "/F"])
            .output();
    }
}

/// T-T7.1.2, native plan step 8 ("a box still alive is taken back, as on Linux"): do: make and start an
/// `oci` box on a node's runtime whose boxes outlive it, run a program in it, and drop the runtime, every
/// handle the node held closed, as when it is killed; then start the next runtime on the node's name and
/// state folder and take the box back. Expect: the container outlives the node, the next runtime takes the
/// box back running, a program runs in it, and its delete leaves no container, record or attached scratch.
#[test]
fn a_nodes_oci_box_outlives_its_runtime_and_the_next_takes_it_back() {
    let Some(lab) = Lab::new() else {
        eprintln!("SKIP: HEX_WINDOWS_LAYERS names no image layers");
        return;
    };
    let _one = one_at_a_time();
    let state = tempfile::tempdir().unwrap();
    let name = format!("t{}-oci-back", std::process::id());
    let id = BoxId::new("hex-oci-kept").unwrap();
    let spec = lab.spec(id.clone(), Bounds::default());
    let markers = state.path().join("boxes");
    let marker = markers.join(format!("{id}.scratch.json"));
    let scratch = lab
        .root
        .path()
        .join(format!("scratch-{id}"))
        .to_str()
        .unwrap()
        .to_owned();
    let _left = Leftover {
        id: &id,
        markers: &markers,
    };
    let node = || {
        WinRuntime::outliving(&name, state.path(), &keeper())
            .unwrap()
            .with_launcher(lab.bin.clone())
    };
    let rt = node();
    let _kept = Kept(rt.keeper_pid().expect("a keeper holds the node's boxes"));
    rt.create(spec.clone()).expect("create");
    rt.start(&id).expect("start");
    let before = run(&rt, &lab, &id, &["say", "before"]);
    assert_eq!(before.0, Exit::Code(0), "{before:?}");
    drop(rt);
    outlived(&id);
    let rt = node();
    rt.adopt(&spec).expect("the next node takes the box back");
    assert_eq!(rt.inspect(&id).unwrap().state, BoxState::Running);
    let after = run(&rt, &lab, &id, &["say", "after"]);
    assert_eq!(after.0, Exit::Code(0), "{after:?}");
    rt.delete(&id).expect("delete");
    assert!(!hcs_holds(&id), "HCS still holds {id} after its delete");
    assert!(!marker.exists(), "the box's record outlived its delete");
    assert!(
        oci::sys::layer_mount_path(&scratch).is_err(),
        "the box's scratch is still attached after its delete"
    );
}

/// T-T7.1.2, native plan step 8 (recovery "cleans up each ended one ... and leaves nothing of an ended
/// box"): do: make and start an `oci` box on a node's runtime whose boxes outlive it and drop the runtime;
/// then end the box from the next runtime on the node's name, as recovery does a box it removes, and end
/// it again. Expect: the container outlives the node; the end leaves no container, record, attached
/// scratch or scratch folder; the second end, of nothing, is accepted.
#[test]
fn an_oci_box_no_node_takes_back_is_ended_with_its_container_and_scratch() {
    let Some(lab) = Lab::new() else {
        eprintln!("SKIP: HEX_WINDOWS_LAYERS names no image layers");
        return;
    };
    let _one = one_at_a_time();
    let state = tempfile::tempdir().unwrap();
    let name = format!("t{}-oci-end", std::process::id());
    let id = BoxId::new("hex-oci-left").unwrap();
    let spec = lab.spec(id.clone(), Bounds::default());
    let markers = state.path().join("boxes");
    let marker = markers.join(format!("{id}.scratch.json"));
    let folder = lab.root.path().join(format!("scratch-{id}"));
    let scratch = folder.to_str().unwrap().to_owned();
    let _left = Leftover {
        id: &id,
        markers: &markers,
    };
    let node = || {
        WinRuntime::outliving(&name, state.path(), &keeper())
            .unwrap()
            .with_launcher(lab.bin.clone())
    };
    let rt = node();
    let _kept = Kept(rt.keeper_pid().expect("a keeper holds the node's boxes"));
    rt.create(spec).expect("create");
    rt.start(&id).expect("start");
    drop(rt);
    outlived(&id);
    let rt = node();
    rt.end(&id).expect("the next node ends the box");
    assert!(!hcs_holds(&id), "HCS still holds {id} after its end");
    assert!(!marker.exists(), "the box's record outlived its end");
    assert!(
        oci::sys::layer_mount_path(&scratch).is_err(),
        "the box's scratch is still attached after its end"
    );
    assert!(
        !folder.exists(),
        "the box's scratch folder outlived its end"
    );
    rt.end(&id).expect("a second end, of nothing");
}

/// A node opened as `hex node` opens one on Windows (crates/hex-cli/src/node.rs): its boxes' records in
/// `state`, its runtime's boxes outliving it, held by the keeper whose state is `state\keeper` and the node
/// named for that folder. Not yet recovered: the test calls [`Recorded::recover`].
fn recorded(lab: &Lab, state: &Path) -> Recorded<WinRuntime> {
    let keeping = state.join("keeper");
    let runtime = WinRuntime::outliving(
        &hex_runtime::windows::keeper::node_name(&keeping),
        &keeping,
        &keeper(),
    )
    .unwrap()
    .with_launcher(lab.bin.clone());
    Recorded::open(runtime, state).unwrap()
}

/// The keeper holding a recorded node's boxes, whichever one owns `state\keeper` when the test ends, asked
/// to stop, so no box's job is held past the test (as tests/windows_node_recovery.rs's guard does).
struct Keepers(PathBuf);

impl Drop for Keepers {
    fn drop(&mut self) {
        let Ok(text) = std::fs::read_to_string(self.0.join("keeper.owner")) else {
            return;
        };
        let mut words = text.split_whitespace();
        let (Some(Ok(pid)), Some(Ok(created))) = (
            words.next().map(str::parse::<u32>),
            words.next().map(str::parse::<u64>),
        ) else {
            return;
        };
        if let Ok(keeper) = hex_runtime::windows::keeper::NodeKeeper::open(pid, created, &self.0) {
            keeper.stop();
        }
    }
}

/// T-T7.1.2, native plan step 8 ("a box still alive is taken back, as on Linux"), through the records
/// `hex node` keeps: do: make and start an `oci` box on a node opened as `hex node` opens one, and drop the
/// node, every handle it held closed, as when it is killed; then open the next node on its state folder and
/// recover. Expect: the container outlives the node; the recovery takes the box back, adopted and running,
/// rather than removing it; a program runs in it; and its delete leaves no container, record or attached
/// scratch.
#[test]
fn a_recorded_nodes_running_oci_box_is_taken_back_by_the_next_nodes_recovery() {
    let Some(lab) = Lab::new() else {
        eprintln!("SKIP: HEX_WINDOWS_LAYERS names no image layers");
        return;
    };
    let _one = one_at_a_time();
    let state = tempfile::tempdir().unwrap();
    let id = BoxId::new("hex-oci-recovered").unwrap();
    let spec = lab.spec(id.clone(), Bounds::default());
    let markers = state.path().join("keeper").join("boxes");
    let marker = markers.join(format!("{id}.scratch.json"));
    let scratch = lab
        .root
        .path()
        .join(format!("scratch-{id}"))
        .to_str()
        .unwrap()
        .to_owned();
    let _left = Leftover {
        id: &id,
        markers: &markers,
    };
    let _keepers = Keepers(state.path().join("keeper"));
    let node = recorded(&lab, state.path());
    node.create(spec).expect("create");
    node.start(&id).expect("start");
    drop(node);
    outlived(&id);
    let node = recorded(&lab, state.path());
    let summary = node.recover().expect("the next node recovers");
    assert_eq!(summary.adopted, std::slice::from_ref(&id), "{summary:?}");
    let info = node.inspect(&id).expect("inspect");
    assert_eq!(
        (info.state, info.adopted),
        (BoxState::Running, true),
        "{info:?}"
    );
    let after = run(&node, &lab, &id, &["say", "after"]);
    assert_eq!(after.0, Exit::Code(0), "{after:?}");
    node.delete(&id).expect("delete");
    assert!(!hcs_holds(&id), "HCS still holds {id} after its delete");
    assert!(!marker.exists(), "the box's record outlived its delete");
    assert!(
        oci::sys::layer_mount_path(&scratch).is_err(),
        "the box's scratch is still attached after its delete"
    );
}

/// T-T7.1.2, native plan step 8 (a box made and not started is taken back as made,
/// `hex_core::hcs::TakeBack::Created`), through the records `hex node` keeps: do: make an `oci` box on a
/// node opened as `hex node` opens one and drop the node before the box's start; then open the next node on
/// its state folder, recover, and start the box there. Expect: the container outlives the node; the recovery
/// takes the box back, adopted and waiting for its start, rather than removing it; its start on the next
/// node runs its init, a program runs in it, and its delete leaves no container, record or attached scratch.
#[test]
fn a_recorded_nodes_oci_box_made_and_not_started_is_taken_back_as_made() {
    let Some(lab) = Lab::new() else {
        eprintln!("SKIP: HEX_WINDOWS_LAYERS names no image layers");
        return;
    };
    let _one = one_at_a_time();
    let state = tempfile::tempdir().unwrap();
    let id = BoxId::new("hex-oci-made").unwrap();
    let spec = lab.spec(id.clone(), Bounds::default());
    let markers = state.path().join("keeper").join("boxes");
    let marker = markers.join(format!("{id}.scratch.json"));
    let scratch = lab
        .root
        .path()
        .join(format!("scratch-{id}"))
        .to_str()
        .unwrap()
        .to_owned();
    let _left = Leftover {
        id: &id,
        markers: &markers,
    };
    let _keepers = Keepers(state.path().join("keeper"));
    let node = recorded(&lab, state.path());
    node.create(spec).expect("create");
    drop(node);
    outlived(&id);
    let node = recorded(&lab, state.path());
    let summary = node.recover().expect("the next node recovers");
    assert_eq!(summary.adopted, std::slice::from_ref(&id), "{summary:?}");
    let info = node.inspect(&id).expect("inspect");
    assert_eq!(
        (info.state, info.adopted),
        (BoxState::Created, true),
        "{info:?}"
    );
    node.start(&id).expect("the next node starts the box");
    assert_eq!(node.inspect(&id).expect("inspect").state, BoxState::Running);
    let after = run(&node, &lab, &id, &["say", "after"]);
    assert_eq!(after.0, Exit::Code(0), "{after:?}");
    node.delete(&id).expect("delete");
    assert!(!hcs_holds(&id), "HCS still holds {id} after its delete");
    assert!(!marker.exists(), "the box's record outlived its delete");
    assert!(
        oci::sys::layer_mount_path(&scratch).is_err(),
        "the box's scratch is still attached after its delete"
    );
}

/// T-T7.1.2, native plan step 8 (recovery "cleans up each ended one ... and leaves nothing of an ended
/// box"), through the records `hex node` keeps: do: make and start an `oci` box on a node opened as `hex
/// node` opens one and drop the node; end the box's init while no node runs, its container left running (an
/// outliving container doesn't end with its init); then open the next node on its state folder and recover.
/// Expect: the recovery succeeds and removes the box, whose init ended
/// (`hex_core::hcs::Lost::InitEnded`), rather than taking back a container with no init, and leaves no
/// container, record, attached scratch or scratch folder.
#[test]
fn a_recorded_nodes_oci_box_whose_init_ended_with_no_node_is_removed_by_the_next_nodes_recovery() {
    let Some(lab) = Lab::new() else {
        eprintln!("SKIP: HEX_WINDOWS_LAYERS names no image layers");
        return;
    };
    let _one = one_at_a_time();
    let state = tempfile::tempdir().unwrap();
    let id = BoxId::new("hex-oci-init-ended").unwrap();
    let spec = lab.spec(id.clone(), Bounds::default());
    let markers = state.path().join("keeper").join("boxes");
    let marker = markers.join(format!("{id}.scratch.json"));
    let folder = lab.root.path().join(format!("scratch-{id}"));
    let scratch = folder.to_str().unwrap().to_owned();
    let _left = Leftover {
        id: &id,
        markers: &markers,
    };
    let _keepers = Keepers(state.path().join("keeper"));
    let node = recorded(&lab, state.path());
    node.create(spec).expect("create");
    node.start(&id).expect("start");
    drop(node);
    outlived(&id);
    // The init ends while no node runs: the record's very process (its creation time told apart from a
    // process given its pid since), ended as a crash ends it and waited for. The handle held on it keeps its
    // pid from being given to another process before the end.
    let init = oci::read_marker(&marker)
        .expect("the box's record")
        .init
        .expect("a started box's record names its init");
    let process = hex_runtime::windows::sys::open_process(init.pid)
        .expect("opening the box's init")
        .expect("the box's init runs");
    assert_eq!(
        hex_runtime::windows::sys::created(&process).expect("its creation time"),
        init.created,
        "the init's pid is another process's"
    );
    let ended = std::process::Command::new("taskkill")
        .args(["/PID", &init.pid.to_string(), "/F"])
        .output()
        .expect("taskkill");
    assert!(
        ended.status.success(),
        "taskkill didn't end the box's init: {ended:?}"
    );
    exit_of(&process, conformance::PATIENCE);
    drop(process);
    assert!(
        oci::sys::open_system(id.as_str())
            .expect("asking HCS for the box's container")
            .is_some(),
        "the box's container ended with its init"
    );
    let node = recorded(&lab, state.path());
    let summary = node.recover().expect("the next node recovers");
    let removed: Vec<&BoxId> = summary.removed.iter().map(|(id, _)| id).collect();
    assert_eq!(removed, [&id], "{summary:?}");
    assert!(summary.adopted.is_empty(), "{summary:?}");
    assert!(
        matches!(node.inspect(&id), Err(Refusal::NoSuchBox(_))),
        "the removed box is still listed"
    );
    assert!(
        !hcs_holds(&id),
        "HCS still holds {id} after its recovery removed it"
    );
    assert!(!marker.exists(), "the box's record outlived its removal");
    assert!(
        oci::sys::layer_mount_path(&scratch).is_err(),
        "the box's scratch is still attached after its removal"
    );
    assert!(
        !folder.exists(),
        "the box's scratch folder outlived its removal"
    );
}

/// A `hex node` of this build on `dir` (its state `dir\state`, its socket `dir\node.sock`), as
/// tests/windows_node_recovery.rs starts one; dropped, it is ended by `TerminateProcess` (std's kill on
/// Windows), as a crash ends it.
struct HexNode {
    child: std::process::Child,
    socket: PathBuf,
}

impl HexNode {
    fn start(dir: &Path) -> Self {
        let socket = dir.join("node.sock");
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_hex"))
            .arg("node")
            .arg("--socket")
            .arg(&socket)
            .arg("--state")
            .arg(dir.join("state"))
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("start hex node");
        let mut line = String::new();
        std::io::BufRead::read_line(
            &mut std::io::BufReader::new(child.stdout.take().unwrap()),
            &mut line,
        )
        .unwrap();
        assert_eq!(line.trim(), format!("ready {}", socket.display()));
        Self { child, socket }
    }

    fn client(&self) -> hex_client::Client {
        hex_client::Client::connect(&self.socket).expect("connect to hex node")
    }
}

impl Drop for HexNode {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// T-T7.1.2, native plan step 8 ("a box still alive is taken back, as on Linux"), through `hex node`
/// itself: do: start `hex node` on a state folder, make and start an `oci` box through its socket, and kill
/// the node (`TerminateProcess`), as a crash ends it; then start `hex node` again on the same state folder.
/// Expect: the container outlives the node; the next node takes the box back, adopted and running; a program
/// runs in it through the socket (its exit code its own); and its delete leaves no container, record or
/// attached scratch. The program's stdio is the null device: a Windows client lends no handles through the
/// socket until the stdio record's winner carries them.
#[test]
fn an_oci_box_outlives_a_killed_hex_node_and_the_next_takes_it_back() {
    let Some(lab) = Lab::new() else {
        eprintln!("SKIP: HEX_WINDOWS_LAYERS names no image layers");
        return;
    };
    let _one = one_at_a_time();
    let dir = tempfile::tempdir().unwrap();
    let id = BoxId::new("hex-oci-node").unwrap();
    let spec = lab.spec(id.clone(), Bounds::default());
    let keeping = dir.path().join("state").join("keeper");
    let markers = keeping.join("boxes");
    let marker = markers.join(format!("{id}.scratch.json"));
    let scratch = lab
        .root
        .path()
        .join(format!("scratch-{id}"))
        .to_str()
        .unwrap()
        .to_owned();
    let _left = Leftover {
        id: &id,
        markers: &markers,
    };
    let _keepers = Keepers(keeping.clone());
    let node = HexNode::start(dir.path());
    let client = node.client();
    client.create(spec).expect("create");
    client.start(&id).expect("start");
    drop(client);
    drop(node);
    outlived(&id);
    let node = HexNode::start(dir.path());
    let client = node.client();
    let info = client
        .inspect(&id)
        .expect("the next node takes the box back");
    assert_eq!(
        (info.state, info.adopted),
        (BoxState::Running, true),
        "{info:?}"
    );
    let mut program = client
        .exec(&id, lab.program(&Act::Fail(3)), Stdio::null())
        .expect("exec");
    let exit = program
        .wait_for(conformance::PATIENCE)
        .expect("wait")
        .expect("the program ends");
    assert_eq!(exit, Exit::Code(3));
    client.delete(&id).expect("delete");
    assert!(!hcs_holds(&id), "HCS still holds {id} after its delete");
    assert!(!marker.exists(), "the box's record outlived its delete");
    assert!(
        oci::sys::layer_mount_path(&scratch).is_err(),
        "the box's scratch is still attached after its delete"
    );
}
