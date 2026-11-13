//! Goal 5 extended, step 1 (decision 2's sharing, with the node agent as its holder; AC-T7.1.4,
//! AC-T7.1.5, T-T7.1.2): each device policy's program is loaded once and held by the node, and the
//! default policy's is attached once, at the node's start, to the root its `oci` boxes alone live under
//! (with `BPF_F_ALLOW_OVERRIDE`). As root, "do X, expect Y" (CLAUDE.md §6):
//! - a box with the default policy, one whose policy allows every device, one whose policy also allows
//!   writing `/dev/kmsg` (char 1:11), a box whose policy denies c 1:3, and a `process` box open devices:
//!   each open is allowed, or refused (EPERM), exactly as its box's policy says; the default box's create
//!   loaded and attached nothing, and only the oci boxes' root carries the default;
//! - a node restarted over boxes made before it: the oci boxes' root carries the same program (its id),
//!   which the restart didn't attach again, and boxes made before and after the restart get the same
//!   decisions;
//!
//! A holder keeps a root's program, and the command line takes a hinted one, only where the kernel proves a loaded
//! program by its translation (`programs::shown`): a kernel that withholds a blinded program's dump from a caller
//! without raw-dump rights (`kernel.kptr_restrict` 2, as the root lab's 7.3.3-1-cachyos on 00aad8da) or rewrites it
//! proves nothing, so there each replaces or loads a copy of its own, and every box keeps its policy all the same. The
//! cases take what they expect of keeping from what the kernel shows, and say it.
//! - N creates with one other policy load its program once; boxes of more policies than the node's bound
//!   close the program used longest ago (the node keeps no descriptor of it), and every box keeps its
//!   policy;
//! - the command line, with a hint planted that names another policy's program: the candidate is refused
//!   by its translation and a copy loaded; the next create with that policy attaches that copy, loading
//!   none.
//!
//! The box whose policy denies c 1:3 can't come from a spec: a runtime's own devices are always allowed.
//! The runtime spec makes a runtime supply /dev/null (c 1:3) and the rest whatever a spec lists
//! (config-linux.md, "Default Devices"), and Hex allows them after any list (`resources.rs`,
//! `DEFAULT_DEVICES`), as runc does ("Append the default allowed devices to the end of the list",
//! libcontainer/specconv/spec_linux.go:984 on main) and crun does (src/libcrun/cgroup-resources.c,
//! `default_devices`). So that policy is built as the runtime builds any, a device state compiled to its
//! program (every device but c 1:3), and given to a cgroup under the node's oci boxes' root by a holder of
//! the node's kind (`Programs::attach`); a program run in that cgroup opens /dev/zero and is refused
//! /dev/null, its own program deciding in the root's place. The box whose policy allows writing /dev/kmsg
//! shows the same from a spec: the root's default would refuse it /dev/kmsg.
//!
//! Root only: loading a device program needs CAP_BPF and CAP_SYS_ADMIN, which a rootless run lacks, so it
//! prints SKIP there; the seam root lab (bench/oci-validation/seam_root.sh) runs it as root.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::io::Read;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::Command;

use hex_api::{Bounds, BoxId, BoxSpec, ClassSpec, Program, Runtime, Stdio};
use hex_node::{Node, OCI_BOXES};
use hex_runtime::linux::cgroup::BoxRoot;
use hex_runtime::linux::oci::devshare::{self, HINTS_FILE, Hints};
use hex_runtime::linux::oci::programs::{self, Programs, Shown};
use hex_runtime::linux::oci::resources::{
    self, ACC_MKNOD, ACC_READ, ACC_WRITE, DEV_CHAR, DeviceState, Exception, Plan,
};
use hex_runtime::linux::oci::run::Runtime as OciRuntime;
use hex_runtime::linux::oci::spec::DeviceRule;
use hex_runtime::linux::oci::sys::{self, BPF_F_ALLOW_OVERRIDE};
use hex_runtime::linux::{disks, process};

/// A policy that allows every device.
const ALLOW_ALL: &str = r#"[{"allow": true, "access": "rwm"}]"#;
/// The default policy's devices, and writing /dev/kmsg besides.
const KMSG: &str = r#"[{"allow": false, "access": "rwm"},
  {"allow": true, "type": "c", "major": 1, "minor": 11, "access": "w"}]"#;
/// Two more policies, each its own program: past a bound of two, the first program held is closed.
const KMSG_FUSE: &str = r#"[{"allow": false, "access": "rwm"},
  {"allow": true, "type": "c", "major": 1, "minor": 11, "access": "w"},
  {"allow": true, "type": "c", "major": 10, "minor": 229, "access": "rw"}]"#;
const KMSG_RW: &str = r#"[{"allow": false, "access": "rwm"},
  {"allow": true, "type": "c", "major": 1, "minor": 11, "access": "rw"}]"#;

/// The devices each box opens: /dev/null and /dev/zero (supplied to every container), /dev/kmsg (which
/// only some policies allow; opened to write, which needs no CAP_SYSLOG), and a block node of no device
/// (b 240:0, a major devices.txt sets aside for local use), whose open a device program refuses (EPERM)
/// before the kernel finds no device there (ENXIO).
const OPENS: [&str; 4] = ["w:/dev/null", "r:/dev/zero", "w:/dev/kmsg", "r:/dev/hexblk"];

/// An open's outcome, as `hex selftest-box open` reports it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Open {
    Ok,
    /// EPERM: the box's device program refused it.
    Refused,
    /// ENXIO: allowed past the device programs, with no device behind the node.
    NoDevice,
}
use Open::{NoDevice, Ok as Opened, Refused};

const TEST: &str = "device_programs_are_held_per_policy_and_the_default_sits_at_the_oci_root";

#[test]
fn device_programs_are_held_per_policy_and_the_default_sits_at_the_oci_root() {
    if host_id("-u") != 0 {
        eprintln!(
            "SKIP {TEST}: device programs need root (CAP_BPF, CAP_SYS_ADMIN); the seam root lab runs this"
        );
        return;
    }
    let exe = env!("CARGO_BIN_EXE_hex").to_owned();
    let cache = cache_dir();
    let own = match prepare_cgroup(&cache) {
        Ok(own) => own,
        Err(why) => {
            eprintln!("SKIP {TEST}: no cgroup v2 subtree of this process's own ({why})");
            return;
        }
    };
    let lab = Lab::new(&cache, &exe);
    // Each case says what it saw, so a lab's record shows every one ran.
    for (case, saw) in [
        ("policies", each_box_is_decided_by_its_policy(&lab, &own)),
        (
            "restart",
            a_restarted_node_keeps_its_roots_program(&lab, &own),
        ),
        (
            "bound",
            a_policy_is_loaded_once_and_the_bound_closes_the_oldest(&lab, &own),
        ),
        (
            "command line",
            a_planted_hint_is_refused_by_its_translation(&lab, &own),
        ),
    ] {
        eprintln!("oci_devices {case}: {saw}");
    }
}

/// Do: make a box with the default policy, then boxes whose policies allow every device and allow
/// writing /dev/kmsg, a `process` box, and a box whose policy denies c 1:3; open the devices in each.
/// Expect: the default box opens /dev/null and /dev/zero and is refused /dev/kmsg and the block node; the
/// allow-all box opens them all (the block node has no device); the kmsg box opens /dev/kmsg, which the
/// root's default would refuse, and is refused the block node; the process box opens /dev/kmsg; the c 1:3
/// box opens /dev/zero and is refused /dev/null, which the root's default allows. The default box's create
/// loaded and attached nothing; the oci boxes' root carries one program, attached with OVERRIDE, and the
/// default box none of its own; the box root, which the process boxes live under, carries none.
fn each_box_is_decided_by_its_policy(lab: &Lab, own: &Path) -> String {
    let state = tempfile::tempdir_in(lab.dir.path()).unwrap();
    let box_root = own.join("hex-dev-policies");
    sweep(&box_root);
    let node = lab.node(&box_root, state.path());
    let at_start = node.device_programs().unwrap();
    assert!(
        at_start.root_attached,
        "a node's first start attaches the default to the oci boxes' root: {at_start:?}"
    );
    let default = make(&node, "dev-default", lab.bundle(None, None));
    let made = node.device_programs().unwrap();
    assert_eq!(
        (made.loaded, made.attached),
        (0, 0),
        "a default box's create loads and attaches nothing: {made:?}"
    );
    let all = make(&node, "dev-all", lab.bundle(Some(ALLOW_ALL), None));
    let kmsg = make(&node, "dev-kmsg", lab.bundle(Some(KMSG), None));
    let process = BoxId::new("dev-process").unwrap();
    node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: process.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Process,
        owner: None,
    })
    .unwrap();

    assert_eq!(
        opens_in(&node, lab, &default),
        [Opened, Opened, Refused, Refused],
        "the default box"
    );
    assert_eq!(
        opens_in(&node, lab, &all),
        [Opened, Opened, Opened, NoDevice],
        "the allow-all box"
    );
    assert_eq!(
        opens_in(&node, lab, &kmsg),
        [Opened, Opened, Opened, Refused],
        "the kmsg box"
    );
    assert_eq!(
        process_opens(&node, lab, &process, &["w:/dev/null", "w:/dev/kmsg"]),
        [Opened, Opened],
        "the process box"
    );

    // The box whose policy denies c 1:3 (the module's head says why no spec can ask it): a cgroup under the
    // node's oci boxes' root, given that policy's program by a holder of the node's kind, which finds the
    // root's default there and keeps it where the kernel proves it, and replaces it by one attach where not.
    let oci_boxes = BoxRoot::open(node.oci_boxes(), disks::under(state.path()).unwrap()).unwrap();
    let no_null = oci_boxes
        .create(&BoxId::new("dev-no-null").unwrap(), &Bounds::default())
        .unwrap();
    let denies_null = Plan {
        devices: Some(resources::compile(&DeviceState {
            allow_by_default: true,
            exceptions: vec![Exception {
                kind: DEV_CHAR,
                major: 1,
                minor: 3,
                access: ACC_MKNOD | ACC_READ | ACC_WRITE,
            }],
        })),
        devices_allow_by_default: true,
        ..Plan::default()
    };
    let held = Programs::at_root(node.oci_boxes(), 1).unwrap();
    assert_eq!(
        held.counts().unwrap().root_attached,
        !lab.proves,
        "the holder kept the node's default on the root where the kernel proves it, else replaced it"
    );
    held.attach(&no_null, &denies_null, false).unwrap();
    assert_eq!(
        opens_in_cgroup(&no_null, lab, &["r:/dev/zero", "w:/dev/null"]),
        [Opened, Refused],
        "the box whose policy denies c 1:3"
    );
    assert_eq!(
        attached(no_null.dir()).count,
        1,
        "the c 1:3 box's own program"
    );
    no_null.remove().unwrap();

    let oci = box_root.join(OCI_BOXES);
    let root = attached(&oci);
    assert_eq!(
        (root.count, root.flags),
        (1, BPF_F_ALLOW_OVERRIDE),
        "the oci boxes' root: {root:?}"
    );
    assert_eq!(attached(&cgroup_of(&node, "dev-default")).count, 0);
    assert_eq!(attached(&cgroup_of(&node, "dev-all")).count, 1);
    assert_eq!(attached(&cgroup_of(&node, "dev-kmsg")).count, 1);
    assert_eq!(attached(&box_root).count, 0, "the box root carries none");
    assert_eq!(attached(&box_root.join("dev-process")).count, 0);
    let after = node.device_programs().unwrap();
    assert_eq!(
        (after.loaded, after.attached, after.held),
        (2, 2, 2),
        "the allow-all and kmsg policies' programs, each loaded and attached once: {after:?}"
    );
    for id in [&default, &all, &kmsg, &process] {
        node.delete(id).unwrap();
    }
    drop(node);
    sweep(&box_root);
    format!(
        "the root's program {:?} (OVERRIDE) decides for the default box, which loaded and attached none; \
         the allow-all, kmsg and c 1:3 boxes their own; each open as its policy says (the c 1:3 box opens \
         /dev/zero, is refused /dev/null); {after:?}",
        root.ids
    )
}

/// Do: make a default box and a kmsg box, drop the node and open another over the same box root and
/// state, recover, and make one more of each. Expect: both boxes are taken back; the oci boxes' root
/// carries the program the first start attached (the same id), and the restart attached none; the boxes
/// made before and after get the same decisions (T-T7.1.2).
fn a_restarted_node_keeps_its_roots_program(lab: &Lab, own: &Path) -> String {
    let state = tempfile::tempdir_in(lab.dir.path()).unwrap();
    let box_root = own.join("hex-dev-restart");
    sweep(&box_root);
    let oci = box_root.join(OCI_BOXES);
    let first = lab.node(&box_root, state.path());
    let before = [
        make(&first, "before-default", lab.bundle(None, None)),
        make(&first, "before-kmsg", lab.bundle(Some(KMSG), None)),
    ];
    let program = attached(&oci);
    assert_eq!(program.count, 1);
    drop(first);

    let second = lab.node(&box_root, state.path());
    let summary = second.recover().unwrap();
    for id in &before {
        assert!(summary.adopted.contains(id), "{id} taken back: {summary:?}");
    }
    let counts = second.device_programs().unwrap();
    assert_eq!(
        counts.root_attached, !lab.proves,
        "the restarted node kept the default on the root where the kernel proves it, else replaced it: {counts:?}"
    );
    let now = attached(&oci);
    if lab.proves {
        assert_eq!(now, program, "the root's program is the same one");
    } else {
        assert_eq!(
            (now.count, now.flags),
            (1, BPF_F_ALLOW_OVERRIDE),
            "the root's program replaced by one: {now:?}"
        );
        assert_ne!(now.ids, program.ids, "the root's program replaced");
    }
    let after = [
        make(&second, "after-default", lab.bundle(None, None)),
        make(&second, "after-kmsg", lab.bundle(Some(KMSG), None)),
    ];
    for (made_before, made_after, want) in [
        (&before[0], &after[0], [Opened, Opened, Refused, Refused]),
        (&before[1], &after[1], [Opened, Opened, Opened, Refused]),
    ] {
        assert_eq!(opens_in(&second, lab, made_before), want, "{made_before}");
        assert_eq!(opens_in(&second, lab, made_after), want, "{made_after}");
    }
    for id in before.iter().chain(&after) {
        second.delete(id).unwrap();
    }
    drop(second);
    sweep(&box_root);
    format!(
        "the root's program {:?} {} across the restart; boxes before and after alike",
        program.ids,
        if lab.proves {
            "kept, none attached by it"
        } else {
            "replaced by one attach, the kernel proving none"
        }
    )
}

/// Do: on a node holding at most two programs, make four boxes with the kmsg policy, then one box each
/// of two more policies, then one more kmsg box. Expect: the four load the kmsg program once and attach
/// it four times; the third policy closes the program used longest ago, the kmsg one, which no descriptor
/// of this process holds after, while it stays attached to each kmsg box, which still opens /dev/kmsg and
/// is refused the block node; the last kmsg box loads it again.
fn a_policy_is_loaded_once_and_the_bound_closes_the_oldest(lab: &Lab, own: &Path) -> String {
    let state = tempfile::tempdir_in(lab.dir.path()).unwrap();
    let box_root = own.join("hex-dev-bound");
    sweep(&box_root);
    let node = lab
        .node(&box_root, state.path())
        .with_device_program_bound(2)
        .unwrap();
    let kmsg: Vec<BoxId> = (0..4)
        .map(|n| make(&node, &format!("once-{n}"), lab.bundle(Some(KMSG), None)))
        .collect();
    let counts = node.device_programs().unwrap();
    assert_eq!(
        (counts.loaded, counts.attached, counts.held),
        (1, 4, 1),
        "one policy's program loaded once: {counts:?}"
    );
    let program = attached(&cgroup_of(&node, "once-0")).ids;
    assert_eq!(program.len(), 1);
    assert!(holds_program(program[0]), "the node holds the kmsg program");
    for id in &kmsg {
        assert_eq!(
            attached(&cgroup_of(&node, id.as_str())).ids,
            program,
            "{id}"
        );
    }

    let fuse = make(&node, "bound-fuse", lab.bundle(Some(KMSG_FUSE), None));
    let rw = make(&node, "bound-rw", lab.bundle(Some(KMSG_RW), None));
    let counts = node.device_programs().unwrap();
    assert_eq!(
        (counts.loaded, counts.held, counts.closed),
        (3, 2, 1),
        "the third policy closed one: {counts:?}"
    );
    assert!(
        !holds_program(program[0]),
        "the program used longest ago, the kmsg policy's, is closed"
    );
    for id in &kmsg {
        assert_eq!(
            attached(&cgroup_of(&node, id.as_str())).ids,
            program,
            "{id} keeps its program"
        );
        assert_eq!(
            opens_in(&node, lab, id),
            [Opened, Opened, Opened, Refused],
            "{id} keeps its policy"
        );
    }
    let again = make(&node, "once-again", lab.bundle(Some(KMSG), None));
    assert_eq!(node.device_programs().unwrap().loaded, 4, "loaded again");
    assert_eq!(
        opens_in(&node, lab, &again),
        [Opened, Opened, Opened, Refused]
    );
    for id in kmsg.iter().chain([&fuse, &rw, &again]) {
        node.delete(id).unwrap();
    }
    let counts = node.device_programs().unwrap();
    drop(node);
    sweep(&box_root);
    format!(
        "four creates loaded program {} once; the third policy closed it, still attached to its boxes, \
         which kept their policy; loaded again after; {counts:?}",
        program[0]
    )
}

/// Do: through the command line (`hex oci`, its state root a directory of this test's), make a default
/// container, plant a hint naming its program for the kmsg policy's digest, make a kmsg container, then
/// a second one. Expect: the first kmsg container's program isn't the planted one (refused by its
/// translation) and the hints name it now; the container opens /dev/kmsg and is refused the block node
/// (the default program would refuse /dev/kmsg); the second kmsg container's program is that same one,
/// so it loaded none.
fn a_planted_hint_is_refused_by_its_translation(lab: &Lab, own: &Path) -> String {
    let root = tempfile::tempdir_in(lab.dir.path()).unwrap();
    let rel = own.strip_prefix("/sys/fs/cgroup").unwrap();
    let hex = |args: &[&str]| {
        let out = Command::new(&lab.exe)
            .arg("oci")
            .args(args)
            .env("HEX_OCI_ROOT", root.path())
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "hex oci {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    // The command line's create passes its streams on to the container's init, as runc's does, so they are
    // the null device and a file here: a pipe the waiting init held would never end.
    let create = |id: &str, policy: Option<&str>| {
        let bundle = lab.bundle(policy, Some(&rel.join(id)));
        let log = root.path().join(format!("{id}.create.log"));
        let status = Command::new(&lab.exe)
            .args(["oci", "create", "--bundle", bundle.to_str().unwrap(), id])
            .env("HEX_OCI_ROOT", root.path())
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::fs::File::create(&log).unwrap())
            .status()
            .unwrap();
        assert!(
            status.success(),
            "hex oci create {id}: {}",
            std::fs::read_to_string(&log).unwrap_or_default()
        );
        own.join(id)
    };
    let plain = create("cli-plain", None);
    let theirs = attached(&plain).ids;
    assert_eq!(theirs.len(), 1, "the default container's own program");

    // KMSG's rules, as the runtime reads them.
    let rule =
        |allow: bool, kind: Option<&str>, num: Option<(i64, i64)>, access: &str| DeviceRule {
            allow,
            kind: kind.map(str::to_owned),
            major: num.map(|n| n.0),
            minor: num.map(|n| n.1),
            access: Some(access.to_owned()),
        };
    let rules = [
        rule(false, None, None, "rwm"),
        rule(true, Some("c"), Some((1, 11)), "w"),
    ];
    let digest = devshare::digest(&resources::devices(&rules).unwrap().unwrap());
    let mut planted = Hints::default();
    planted.remember(digest, theirs[0]);
    std::fs::write(root.path().join(HINTS_FILE), planted.to_bytes().unwrap()).unwrap();

    let first = create("cli-kmsg", Some(KMSG));
    let ours = attached(&first).ids;
    assert_eq!(ours.len(), 1);
    assert_ne!(ours, theirs, "the planted candidate was refused");
    let hints = Hints::parse(&std::fs::read(root.path().join(HINTS_FILE)).unwrap());
    assert_eq!(
        hints.candidate(digest),
        Some(ours[0]),
        "the copy is remembered"
    );
    let mut args = vec!["exec", "cli-kmsg", "/hex", "selftest-box", "open"];
    args.extend(OPENS);
    assert_eq!(
        parse_opens(&hex(&args)),
        [Opened, Opened, Opened, Refused],
        "the kmsg container"
    );
    let second = create("cli-kmsg2", Some(KMSG));
    let seconds = attached(&second).ids;
    if lab.proves {
        assert_eq!(
            seconds, ours,
            "the second takes the first's program, loading none"
        );
    } else {
        assert_eq!(seconds.len(), 1, "the second's own program");
        assert_ne!(
            seconds, ours,
            "the kernel proves no hint, so the second loads a copy"
        );
    }
    for id in ["cli-plain", "cli-kmsg", "cli-kmsg2"] {
        hex(&["delete", "--force", id]);
    }
    format!(
        "the planted hint's program {theirs:?} refused by its translation, {ours:?} loaded; the next create \
         attached {seconds:?}, {}",
        if lab.proves {
            "loading none"
        } else {
            "a copy, the kernel proving no hint"
        }
    )
}

/// Removes a box root this test left, with its oci boxes' root, so a run starts on a fresh one; one
/// still holding boxes stays.
fn sweep(box_root: &Path) {
    let _ = std::fs::remove_dir(box_root.join(OCI_BOXES));
    let _ = std::fs::remove_dir(box_root);
}

/// Makes and starts box `id` from `bundle`; its init waits.
fn make(node: &Node, id: &str, bundle: PathBuf) -> BoxId {
    let id = BoxId::new(id).unwrap();
    node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Bundle(bundle),
        owner: None,
    })
    .unwrap_or_else(|e| panic!("create {id}: {e}"));
    node.start(&id).unwrap();
    id
}

/// What `oci` box `id` opens of [`OPENS`]: `hex selftest-box open` exec'd into its container through the
/// oci runtime (`hex __oci-exec`, which joins the container's cgroup, so its device programs decide),
/// over the node's container state, so a box a restarted node took back is asked the same way.
fn opens_in(node: &Node, lab: &Lab, id: &BoxId) -> Vec<Open> {
    let dir = node.container_dir(id);
    let oci = OciRuntime::with_helper(node.oci_root().to_owned(), PathBuf::from(&lab.exe));
    let mut args: Vec<String> = ["/hex", "selftest-box", "open"]
        .iter()
        .map(|a| (*a).to_owned())
        .collect();
    args.extend(OPENS.iter().map(|o| (*o).to_owned()));
    let out = oci
        .exec_command(
            dir.file_name().unwrap().to_str().unwrap(),
            args,
            None,
            vec![PATH.to_owned()],
            None,
            false,
        )
        .unwrap()
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{id}'s open: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    parse_opens(&String::from_utf8(out.stdout).unwrap())
}

/// What `process` box `id` opens of `paths`, by `hex selftest-box open` exec'd into it.
fn process_opens(node: &Node, lab: &Lab, id: &BoxId, paths: &[&str]) -> Vec<Open> {
    let (mut out, w) = std::io::pipe().unwrap();
    let mut args = vec![lab.exe.as_str(), "selftest-box", "open"];
    args.extend(paths);
    let mut proc = node
        .exec(
            id,
            Program::of(&args),
            Stdio {
                stdout: Some(w),
                ..Stdio::null()
            },
        )
        .unwrap();
    let mut text = String::new();
    out.read_to_string(&mut text).unwrap();
    proc.wait_for(hex_api::conformance::PATIENCE).unwrap();
    parse_opens(&text)
}

/// What a program started in cgroup `cg` opens of `paths`: `hex selftest-box open`, started there as a
/// `process` box's programs are, on the host's /dev.
fn opens_in_cgroup(
    cg: &hex_runtime::linux::cgroup::BoxCgroup,
    lab: &Lab,
    paths: &[&str],
) -> Vec<Open> {
    use hex_api::Proc;
    let (mut out, w) = std::io::pipe().unwrap();
    let mut args = vec![lab.exe.as_str(), "selftest-box", "open"];
    args.extend(paths);
    let mut proc = process::exec(
        cg,
        &Program::of(&args),
        Stdio {
            stdout: Some(w),
            ..Stdio::null()
        },
    )
    .unwrap();
    let mut text = String::new();
    out.read_to_string(&mut text).unwrap();
    proc.wait_for(hex_api::conformance::PATIENCE).unwrap();
    parse_opens(&text)
}

fn parse_opens(text: &str) -> Vec<Open> {
    text.lines()
        .map(|l| match l.rsplit_once(' ') {
            Some((_, "ok")) => Opened,
            Some((_, "1")) => Refused,
            Some((_, "6")) => NoDevice,
            _ => panic!("an open's outcome: {l:?}"),
        })
        .collect()
}

/// Box `id`'s cgroup, as the node has it: one named by the id, or a slot made ahead (decision 8).
fn cgroup_of(node: &Node, id: &str) -> PathBuf {
    node.cgroup_dir(&BoxId::new(id).unwrap())
        .unwrap_or_else(|| panic!("box {id} has a cgroup"))
}

/// The device programs attached to cgroup `dir` itself.
fn attached(dir: &Path) -> sys::Attached {
    let fd = rustix::fs::open(
        dir,
        rustix::fs::OFlags::DIRECTORY | rustix::fs::OFlags::RDONLY,
        rustix::fs::Mode::empty(),
    )
    .unwrap_or_else(|e| panic!("open {}: {e}", dir.display()));
    sys::attached_device_programs(fd.as_fd()).unwrap()
}

/// Whether a descriptor of this process holds BPF program `id` (`prog_id:` in its fdinfo).
fn holds_program(id: u32) -> bool {
    let want = format!("prog_id:\t{id}");
    std::fs::read_dir("/proc/self/fdinfo").unwrap().any(|fd| {
        std::fs::read_to_string(fd.unwrap().path()).is_ok_and(|t| t.lines().any(|l| l == want))
    })
}

const PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Where one run keeps its bundles and the rootfs they share.
struct Lab {
    exe: String,
    rootfs: PathBuf,
    dir: tempfile::TempDir,
    /// Whether the kernel proves a loaded device program by its translation, which keeping a root's program and
    /// taking a hinted one rest on (`programs::shown`).
    proves: bool,
}

impl Lab {
    fn new(cache: &Path, exe: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("ocidevices-")
            .tempdir_in(cache)
            .unwrap();
        let rootfs = dir.path().join("rootfs");
        build_rootfs(&rootfs);
        let shown = programs::shown(&programs::Kernel).unwrap();
        eprintln!("oci_devices kernel: a device program it loaded shows as {shown:?}");
        Self {
            exe: exe.to_owned(),
            rootfs,
            dir,
            proves: shown == Shown::Itself,
        }
    }

    fn node(&self, box_root: &Path, state: &Path) -> Node {
        Node::open(box_root, state, state)
            .unwrap()
            .with_oci_helper(PathBuf::from(&self.exe))
    }

    /// A bundle whose init waits, as root with no user namespace, /dev/kmsg and the block node made in
    /// its /dev (`linux.devices`), `policy` its `linux.resources.devices` (none: the default policy),
    /// and its cgroup at `cgroup` (relative to the cgroup root) for the command line's create.
    fn bundle(&self, policy: Option<&str>, cgroup: Option<&Path>) -> PathBuf {
        let dir = tempfile::Builder::new()
            .prefix("bundle-")
            .tempdir_in(self.dir.path())
            .unwrap()
            .keep();
        let resources = policy.map_or(String::new(), |p| {
            format!(r#", "resources": {{"devices": {p}}}"#)
        });
        let cgroup = cgroup.map_or(String::new(), |c| {
            format!(r#", "cgroupsPath": "/{}""#, c.display())
        });
        let config = format!(
            r#"{{
  "ociVersion": "1.3.0",
  "root": {{"path": "{rootfs}", "readonly": false}},
  "process": {{"cwd": "/", "args": ["/hex", "selftest-box", "wait"], "env": ["{PATH}"],
              "user": {{"uid": 0, "gid": 0}}, "noNewPrivileges": true}},
  "mounts": [
    {{"destination": "/proc", "type": "proc", "source": "proc"}},
    {{"destination": "/dev", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid","mode=755","size=65536k"]}},
    {{"destination": "/tmp", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid","nodev"]}},
    {{"destination": "/usr", "type": "bind", "source": "/usr", "options": ["rbind","ro"]}},
    {{"destination": "/hex", "type": "bind", "source": "{exe}", "options": ["bind","ro"]}}
  ],
  "linux": {{
    "namespaces": [{{"type": "pid"}}, {{"type": "mount"}}, {{"type": "ipc"}}, {{"type": "uts"}}],
    "devices": [
      {{"path": "/dev/kmsg", "type": "c", "major": 1, "minor": 11, "fileMode": 438, "uid": 0, "gid": 0}},
      {{"path": "/dev/hexblk", "type": "b", "major": 240, "minor": 0, "fileMode": 438, "uid": 0, "gid": 0}}
    ]{resources}{cgroup}
  }}
}}"#,
            rootfs = self.rootfs.display(),
            exe = self.exe,
        );
        std::fs::write(dir.join("config.json"), config).unwrap();
        dir
    }
}

/// A rootfs of the host's `/usr`, bound in by each bundle, with the host's top-level links into it
/// (`/bin`, `/lib64`, …) so the loader resolves as on the host.
fn build_rootfs(rootfs: &Path) {
    for d in ["proc", "dev", "sys", "tmp", "usr"] {
        std::fs::create_dir_all(rootfs.join(d)).unwrap();
    }
    std::fs::File::create(rootfs.join("hex")).unwrap();
    for link in ["bin", "sbin", "lib", "lib64"] {
        if let Ok(target) = std::fs::read_link(Path::new("/").join(link)) {
            let target = target.to_string_lossy().trim_start_matches('/').to_owned();
            std::os::unix::fs::symlink(target, rootfs.join(link)).unwrap();
        }
    }
}

fn host_id(flag: &str) -> u32 {
    let out = Command::new("id").arg(flag).output().expect("id");
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("id is a number")
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

/// Moves this process into a leaf of its own cgroup and confirms every controller is delegated, as
/// oci_box.rs does; returns the own cgroup dir, or why it isn't ours to subdivide.
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
    let cache = PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/hex-t7/ocibox");
    std::fs::create_dir_all(&cache).expect("cache dir");
    cache
}

#[path = "oci_devices/native_devices.rs"]
mod native_devices;

#[path = "oci_devices/native_scheduler.rs"]
mod native_scheduler;
