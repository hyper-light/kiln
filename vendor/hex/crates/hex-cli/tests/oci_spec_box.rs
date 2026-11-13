//! Runtime goal 3, plan step 5 (decision 3; AC-T7.1.1, T-T7.1.2): a box made from the structured
//! `OciSpec` runs from the bundle the node renders for it. "do X, expect Y" (§6):
//! - a box from layers [base, app] whose init is `sleep infinity`, with one read-only bind mount, a
//!   hostname, a hosts line and a private network: its root is an overlay; `/usr` is the bind and
//!   read-only; its hostname, `/etc/hosts` line and network namespace are the spec's, the network
//!   holding loopback alone, up; what it writes lands in its own writable layer and the layers are
//!   untouched; its network namespace is counted, as root one the node made ahead and the create took,
//!   rootless one made on its create's path (decision 4); after delete nothing of it is left: no bundle
//!   (so no writable layer), no container state, no cgroup, no record, no mount;
//! - a box from a directory on the host's network: its writes land in the directory, in place, its
//!   network namespace is the node's (none is taken or made for it), and after delete nothing of it is
//!   left but those writes;
//! - the hex-api conformance suite (HEX-RT-1 to 13) on boxes made from OciSpecs;
//! - a spec this node can't render (a root or a mount source that isn't there, a user a rootless
//!   node can't map) is refused by the field's name, and nothing is made;
//! - a crash mid-create leaves a rendered bundle that recovery removes, however its layer was left:
//!   alone or with the box's cgroup, as today's node leaves it with no record yet, or with a record
//!   saying the box was being made, as a node before goal 5 extended step 5 left it (T-T7.1.2).
//!
//! Rootless where the caller isn't root (overlayfs mounts unprivileged since Linux 5.11), as root in a
//! privileged container otherwise (bench/oci-validation's setting); in a delegated cgroup v2 subtree,
//! as oci_box.rs. Where none of that is possible it records the skip and why (hex_testkit, which
//! the gate prints) rather than pass falsely.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Command;

use hex_api::conformance::{self, Act, Fixture};
use hex_api::{
    Bounds, BoxId, BoxSpec, BoxState, ClassSpec, Exit, Mount, Network, OciSpec, Program, Refusal,
    Rootfs, Runtime, Stdio, User,
};
use hex_core::patience::Patience;
use hex_core::record::{BoxRecord, Phase};
use hex_node::{Node, OCI_BOXES, records_dir};

const TEST: &str = "oci_spec_boxes_run_from_the_bundles_the_node_renders";

#[test]
fn oci_spec_boxes_run_from_the_bundles_the_node_renders() {
    let exe = env!("CARGO_BIN_EXE_hex").to_owned();
    let cache = cache_dir();
    match prepare_cgroup(&cache) {
        Ok(own) => exercise(&own, &cache, &exe),
        Err(direct) if std::env::var_os("HEX_DELEGATED").is_none() => {
            let has_systemd = Command::new("systemd-run")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !has_systemd {
                hex_testkit::skip(
                    "oci_spec_boxes_run_from_the_bundles_the_node_renders",
                    "cgroup v2 delegation",
                    &format!(
                        "own cgroup isn't ours to subdivide ({direct}) and no systemd --user for a private scope"
                    ),
                )
                .expect("recording the skip");
                return;
            }
            let status = Command::new("systemd-run")
                .args(["--user", "--scope", "--quiet", "-p", "Delegate=yes"])
                .arg(std::env::current_exe().expect("test binary path"))
                .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
                .env("HEX_DELEGATED", "1")
                .status()
                .expect("run the test in a delegated scope");
            assert!(status.success(), "the delegated re-run failed");
        }
        Err(direct) => {
            hex_testkit::skip(
                "oci_spec_boxes_run_from_the_bundles_the_node_renders",
                "cgroup v2 delegation",
                &format!("none in the delegated scope either ({direct}); run on a cgroup-v2 host"),
            )
            .expect("recording the skip");
        }
    }
}

/// One run's layers, directories and nodes.
struct Lab {
    exe: String,
    work: tempfile::TempDir,
    base: PathBuf,
    app: PathBuf,
    rootless: bool,
}

impl Lab {
    fn new(cache: &Path, exe: &str) -> Self {
        let work = tempfile::Builder::new()
            .prefix("ocispec-")
            .tempdir_in(cache)
            .expect("work dir");
        let base = work.path().join("layers/base");
        build_base(&base);
        let app = work.path().join("layers/app");
        std::fs::create_dir_all(app.join("app")).unwrap();
        std::fs::write(app.join("app/hello"), "hello from the app layer\n").unwrap();
        Self {
            exe: exe.to_owned(),
            work,
            base,
            app,
            rootless: host_id("-u") != 0,
        }
    }

    /// A node over its own box root and state.
    fn node(&self, own: &Path, name: &str) -> (Node, PathBuf, PathBuf) {
        let box_root = own.join(format!("hex-ocispec-{name}"));
        let state = self.work.path().join(format!("state-{name}"));
        std::fs::create_dir_all(&state).unwrap();
        let node = Node::open(&box_root, &state, &state)
            .expect("open a node")
            .with_oci_helper(PathBuf::from(&self.exe));
        (node, box_root, state)
    }

    /// The conformance suite's container: the base layer alone, the host's `/usr` and the `hex`
    /// binary bound in read-only, a private network.
    fn conformance_spec(&self, args: &[String]) -> OciSpec {
        OciSpec {
            rootfs: Rootfs::Layers(vec![self.base.clone()]),
            init: Program::of(&args.iter().map(String::as_str).collect::<Vec<_>>()),
            mounts: vec![usr(), bind(&self.exe, "/hex")],
            hostname: None,
            network: Network::Private,
            hosts: Vec::new(),
        }
    }

    /// The `hex selftest-box` program for `act`, as the container sees the binary (`/hex`).
    fn act(act: &Act) -> Vec<String> {
        let mut args = vec!["/hex".to_owned(), "selftest-box".to_owned()];
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
}

impl Fixture for Lab {
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
            class: ClassSpec::Oci(Box::new(self.conformance_spec(&Self::act(&Act::Wait)))),
            owner: None,
            band: 0,
        }
    }

    fn init(&self, id: BoxId, act: &Act) -> BoxSpec {
        BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id,
            bounds: Bounds::default(),
            class: ClassSpec::Oci(Box::new(self.conformance_spec(&Self::act(act)))),
            owner: None,
            band: 0,
        }
    }

    fn program(&self, act: &Act) -> Program {
        let args = Self::act(act);
        Program::of(&args.iter().map(String::as_str).collect::<Vec<_>>())
    }
}

fn exercise(own: &Path, cache: &Path, exe: &str) {
    let lab = Lab::new(cache, exe);
    eprintln!(
        "oci_spec_box: {} (uid {}), kernel {}",
        if lab.rootless { "rootless" } else { "as root" },
        host_id("-u"),
        std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .unwrap_or_default()
            .trim()
    );
    // A rootless container needs unprivileged user namespaces; a host without them can't run one,
    // which is a reason to skip, said. Past that, any failure is the lab's to report.
    if lab.rootless && !userns_available() {
        hex_testkit::skip(
            "oci_spec_boxes_run_from_the_bundles_the_node_renders",
            "unprivileged user namespaces",
            "this host makes none for an unprivileged user",
        )
        .expect("recording the skip");
        return;
    }
    layered_box(&lab, own);
    in_place_box(&lab, own);
    links_in_a_layer_stay_in_the_box(&lab, own);
    conformance_on_specs(&lab, own);
    refusals(&lab, own);
    recovery_removes_a_half_made_bundle(&lab, own);
}

/// AC-f (goal 5 plan step 2): a started pruned create seed's mount namespace is a read-only tmpfs
/// holding nothing, on the namespace's root, and nothing of the host: no proc, sysfs or cgroup2. Since
/// plan step 3 the init attaches the container's root on the tmpfs's own root, so the tmpfs needs no
/// directory to attach it at. Its mountinfo lists the tmpfs alone, by the kernel's design. The namespace's root, the
/// tmpfs's parent, lies outside the seed's root, and the kernel lists no mount it can't reach from the
/// task's root: v6.19 `show_mountinfo` (fs/proc_namespace.c:150-151, "mountpoints outside of chroot jail
/// will give SEQ_SKIP on this") skips what `seq_path_root` can't name (fs/seq_file.c:517-519), and
/// `__d_path` names nothing unreachable from the root (fs/d_path.c:211-223). The node runs in this process
/// and its create pool spawns `hex __oci-seed` children with `HEX_OCI_SEED_JOIN` set, each joining the node's
/// one pruned namespace (`goal6-mntns.md` §12), so the seed is found among this process's children by that
/// environment entry (an exec seed, unpruned, has none), and its mountinfo and root read. Run only as root,
/// where the node prunes.
fn pruned_seed_lists_its_read_only_root_tmpfs_alone() {
    let me = std::process::id();
    let mut seed = None;
    for entry in std::fs::read_dir("/proc").unwrap().flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        // proc_pid_stat(5): ppid is field 4, after the comm (field 2) in parens, which can hold spaces
        // and parens — so read the fields after the last ')'.
        let after = stat.rsplit_once(')').map(|(_, a)| a).unwrap_or("");
        if after
            .split_whitespace()
            .nth(1)
            .and_then(|p| p.parse::<u32>().ok())
            != Some(me)
        {
            continue;
        }
        let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
        if environ
            .split(|&b| b == 0)
            .any(|e| e.starts_with(b"HEX_OCI_SEED_JOIN="))
        {
            seed = Some(pid);
            break;
        }
    }
    let seed = seed.expect("a pruned create seed among this process's children");
    let mountinfo =
        std::fs::read_to_string(format!("/proc/{seed}/mountinfo")).expect("the seed's mountinfo");
    // proc_pid_mountinfo(5): mount ID, parent ID, major:minor, root, mount point, mount options; after
    // " - ", the filesystem type.
    let mounts: Vec<(Vec<&str>, &str)> = mountinfo
        .lines()
        .map(|line| {
            let (fields, fs) = line.split_once(" - ").expect("a mountinfo line");
            let fstype = fs.split_whitespace().next().unwrap_or("");
            (fields.split_whitespace().collect(), fstype)
        })
        .collect();
    assert!(
        mounts
            .iter()
            .all(|(_, fstype)| !matches!(*fstype, "proc" | "sysfs" | "cgroup2" | "cgroup")),
        "a pruned create seed (pid {seed}) holds no proc, sysfs or cgroup:\n{mountinfo}"
    );
    assert_eq!(
        mounts.len(),
        1,
        "a pruned create seed (pid {seed}) lists its root tmpfs alone, got {}:\n{mountinfo}",
        mounts.len()
    );
    let (fields, fstype) = &mounts[0];
    assert_eq!(
        *fstype, "tmpfs",
        "the seed's one mount is its tmpfs:\n{mountinfo}"
    );
    assert_eq!(fields[4], "/", "the tmpfs is the seed's /:\n{mountinfo}");
    assert!(
        fields[5].split(',').any(|o| o == "ro"),
        "the seed's root tmpfs is mounted read-only:\n{mountinfo}"
    );
    // Its parent, the namespace's root, lies outside the seed's root, so no line lists it.
    assert!(
        mounts.iter().all(|(m, _)| m[0] != fields[1]),
        "the tmpfs's parent (mount {}) isn't listed:\n{mountinfo}",
        fields[1]
    );
    let root: Vec<String> = std::fs::read_dir(format!("/proc/{seed}/root"))
        .expect("the seed's root")
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(root.is_empty(), "the seed's root holds nothing: {root:?}");
    eprintln!(
        "AC-f: the pruned create seed (pid {seed}) lists one mount, its read-only root tmpfs:"
    );
    eprint!("{mountinfo}");
}

/// The plan step's example: layers [base, app], `sleep infinity`, one read-only bind, a private
/// network (with a hostname and a hosts line).
fn layered_box(lab: &Lab, own: &Path) {
    let (node, box_root, state) = lab.node(own, "layers");
    if !lab.rootless {
        pool_holds_one(&node);
    }
    let before = (snapshot(&lab.base), snapshot(&lab.app));
    let id = BoxId::new("layered").unwrap();
    let spec = OciSpec {
        rootfs: Rootfs::Layers(vec![lab.base.clone(), lab.app.clone()]),
        init: Program::of(&["sleep", "infinity"]),
        mounts: vec![usr()],
        hostname: Some("spec-box".to_owned()),
        network: Network::Private,
        hosts: vec![("db_main".to_owned(), "10.1.2.3".to_owned())],
    };
    let made = node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Oci(Box::new(spec)),
        owner: None,
        band: 0,
    });
    assert_eq!(made.expect("create").state, BoxState::Created);
    let _cleanup = Deleting(&node, id.clone());
    // Its private network is a namespace of its own (decision 4): as root, one the node made ahead,
    // which it takes; rootless, made on its create's path in the box's own user namespace.
    assert_eq!(
        boxes_namespaces(&node),
        if lab.rootless { (0, 1) } else { (1, 0) },
        "(taken, made on the path)"
    );
    node.start(&id).expect("start");
    assert_eq!(node.inspect(&id).unwrap().state, BoxState::Running);
    // Goal 5 plan step 2: an eligible box takes no shim fallback (no pid/user namespace by path, no
    // user namespace of its own, no hooks). A root node makes it on the pruned root path (no
    // `pivot_root`); a rootless node's seeds don't prune, so it takes the classic path in the init.
    assert_eq!(
        node.creates_through_shim(),
        0,
        "an eligible box falls back to no shim"
    );
    if lab.rootless {
        assert_eq!(
            node.pruned_creates(),
            0,
            "a rootless node's seeds don't prune their mount namespace"
        );
    } else {
        assert!(
            node.pruned_creates() >= 1,
            "a root node makes an eligible box on the pruned root path, {} made",
            node.pruned_creates()
        );
        // AC-f: a started pruned create seed's mount namespace is a read-only tmpfs on the namespace's
        // root, which its mountinfo lists alone, and no proc, sysfs or cgroup2. The seed is a child of
        // this process (the node runs in-process and its create pool spawns `hex __oci-seed`), so find
        // it and read its mountinfo and root.
        pruned_seed_lists_its_read_only_root_tmpfs_alone();
    }
    let layer = state.join("bundles/layered");

    // Its root is the overlay, over the box's own writable layer; /usr is the bind, read-only.
    let mounts = mountinfo(&run(&node, &id, "cat /proc/self/mountinfo"));
    let root = mounts
        .iter()
        .rev()
        .find(|m| m.point == "/")
        .expect("a root");
    assert_eq!(root.fstype, "overlay", "the root: {root:?}");
    assert!(
        root.super_options
            .contains(&format!("upperdir={}", layer.join("upper").display())),
        "the overlay's writable layer is the box's own: {root:?}"
    );
    let bound = mounts
        .iter()
        .rev()
        .find(|m| m.point == "/usr")
        .expect("/usr");
    assert!(
        bound.options.split(',').any(|o| o == "ro"),
        "/usr is read-only: {bound:?}"
    );
    assert_ne!(
        exit_of(&node, &id, "touch /usr/hex-probe"),
        Exit::Code(0),
        "a write to the read-only bind is refused"
    );
    // Its hostname, hosts line and network are the spec's: its own network namespace, holding
    // loopback alone, up (IFF_UP, 0x1).
    assert_eq!(
        run(&node, &id, "cat /proc/sys/kernel/hostname").trim(),
        "spec-box"
    );
    let hosts = run(&node, &id, "cat /etc/hosts");
    assert!(
        hosts.starts_with("127.0.0.1\tlocalhost\n") && hosts.ends_with("10.1.2.3\tdb_main\n"),
        "/etc/hosts: {hosts:?}"
    );
    assert_ne!(
        run(&node, &id, "readlink /proc/self/ns/net").trim(),
        own_ns("net"),
        "a private network is a namespace of the box's own"
    );
    assert_eq!(run(&node, &id, "ls /sys/class/net").trim(), "lo");
    let flags = run(&node, &id, "cat /sys/class/net/lo/flags");
    let flags = u32::from_str_radix(flags.trim().trim_start_matches("0x"), 16).unwrap();
    assert_eq!(flags & 0x1, 0x1, "loopback is up: flags {flags:#x}");

    // Writes land in the box's own layer: a new file, a layer's file changed and one removed; the
    // layers are untouched.
    assert_eq!(
        exit_of(
            &node,
            &id,
            "echo written > /written && echo changed > /app/hello && rm /base-file"
        ),
        Exit::Code(0)
    );
    let upper = layer.join("upper");
    assert_eq!(
        std::fs::read_to_string(upper.join("written")).unwrap(),
        "written\n"
    );
    assert_eq!(
        std::fs::read_to_string(upper.join("app/hello")).unwrap(),
        "changed\n"
    );
    let whiteout = std::fs::symlink_metadata(upper.join("base-file")).unwrap();
    assert!(
        whiteout.file_type().is_char_device() && whiteout.rdev() == 0,
        "a removed layer file is a whiteout in the box's layer"
    );
    assert_eq!(
        (snapshot(&lab.base), snapshot(&lab.app)),
        before,
        "the layers are untouched"
    );

    node.delete(&id).expect("delete");
    nothing_left(&node, &id, &box_root, &state);
    assert_eq!(
        (snapshot(&lab.base), snapshot(&lab.app)),
        before,
        "the layers are untouched by delete"
    );
}

/// A directory root is used in place: the box's writes land in it, and after delete they're all
/// that's left of the box. On the host's network, its network namespace is the node's.
fn in_place_box(lab: &Lab, own: &Path) {
    let (node, box_root, state) = lab.node(own, "dir");
    let dir = lab.work.path().join("in-place");
    build_base(&dir);
    let id = BoxId::new("in-place").unwrap();
    node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Oci(Box::new(OciSpec {
            rootfs: Rootfs::Dir(dir.clone()),
            init: Program::of(&["sleep", "infinity"]),
            mounts: vec![usr()],
            hostname: None,
            network: Network::Host,
            hosts: Vec::new(),
        })),
        owner: None,
        band: 0,
    })
    .expect("create");
    let _cleanup = Deleting(&node, id.clone());
    assert_eq!(
        boxes_namespaces(&node),
        (0, 0),
        "the host's network is no namespace of the box's own"
    );
    node.start(&id).expect("start");
    assert_eq!(
        exit_of(&node, &id, "echo in-place > /written-in-place"),
        Exit::Code(0)
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("written-in-place")).unwrap(),
        "in-place\n"
    );
    assert_eq!(
        run(&node, &id, "readlink /proc/self/ns/net").trim(),
        own_ns("net"),
        "the host's network is the node's namespace"
    );
    assert_eq!(
        run(&node, &id, "cat /proc/self/cgroup").trim(),
        "0::/",
        "the box's cgroup is the root of its cgroup namespace"
    );
    node.delete(&id).expect("delete");
    nothing_left(&node, &id, &box_root, &state);
    assert!(
        dir.join("written-in-place").exists(),
        "a directory root keeps what was written in it"
    );
}

/// An image's links stay in its box. A layer whose `/etc` is an absolute link to a host directory:
/// the node writes nothing there, refuses the box by the path that leads out, and leaves nothing of
/// it. A layer whose `/etc` is an absolute link to a directory in the image: followed there, inside
/// the box's root, so the box's `/etc/hosts` is made in its own layer and the box sees it.
fn links_in_a_layer_stay_in_the_box(lab: &Lab, own: &Path) {
    let probe = lab.work.path().join("escape-probe");
    std::fs::create_dir_all(&probe).unwrap();
    let out = lab.work.path().join("layers/link-out");
    std::fs::create_dir_all(&out).unwrap();
    std::os::unix::fs::symlink(&probe, out.join("etc")).unwrap();
    let inside = lab.work.path().join("layers/link-inside");
    std::fs::create_dir_all(inside.join("real-etc")).unwrap();
    std::os::unix::fs::symlink("/real-etc", inside.join("etc")).unwrap();
    let (node, box_root, state) = lab.node(own, "links");
    let spec = |layer: &Path| {
        ClassSpec::Oci(Box::new(OciSpec {
            rootfs: Rootfs::Layers(vec![lab.base.clone(), layer.to_path_buf()]),
            init: Program::of(&["sleep", "infinity"]),
            mounts: vec![usr()],
            hostname: None,
            network: Network::Private,
            hosts: vec![("db".to_owned(), "10.1.2.4".to_owned())],
        }))
    };
    let wrote = |probe: &Path| -> Vec<PathBuf> {
        std::fs::read_dir(probe)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect()
    };

    let id = BoxId::new("link-out").unwrap();
    let made = node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: spec(&out),
        owner: None,
        band: 0,
    });
    let _cleanup = Deleting(&node, id.clone());
    assert_eq!(
        wrote(&probe),
        Vec::<PathBuf>::new(),
        "the node wrote through a layer's link onto the host"
    );
    assert!(
        matches!(&made, Err(Refusal::Os { why, .. })
            if why.contains("/etc/hosts: etc in the container's root is a link to nothing inside it")),
        "{made:?}"
    );
    nothing_left(&node, &id, &box_root, &state);

    let id = BoxId::new("link-inside").unwrap();
    node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: spec(&inside),
        owner: None,
        band: 0,
    })
    .expect("create");
    let _cleanup = Deleting(&node, id.clone());
    node.start(&id).expect("start");
    let hosts = run(&node, &id, "cat /etc/hosts");
    assert!(hosts.ends_with("10.1.2.4\tdb\n"), "/etc/hosts: {hosts:?}");
    assert!(
        state
            .join("bundles/link-inside/upper/real-etc/hosts")
            .is_file(),
        "the bind's target is made in the box's own layer, where the link leads"
    );
    node.delete(&id).expect("delete");
    nothing_left(&node, &id, &box_root, &state);
    assert_eq!(wrote(&probe), Vec::<PathBuf>::new());
}

/// AC-T7.1.1: the hex-api conformance suite holds for boxes made from OciSpecs, HEX-RT-13 (an
/// unrenderable spec refused by name) included, and the differential suite holds them to the
/// simulator's trace (HEX-DIFF-1..6, CLAUDE.md §8).
fn conformance_on_specs(lab: &Lab, own: &Path) {
    let (node, _, _) = lab.node(own, "conformance");
    let failures = hex_api::differential::run(&node, lab);
    assert!(failures.is_empty(), "conformance failures: {failures:#?}");
}

/// What this node can't render is refused by the field's name, and nothing is made: a root or a
/// mount source that isn't there, a layer that isn't a directory, and, rootless, a user the node
/// can't map into the container.
fn refusals(lab: &Lab, own: &Path) {
    let (node, box_root, state) = lab.node(own, "refusals");
    let id = BoxId::new("refused").unwrap();
    let missing = lab.work.path().join("missing");
    let mut cases: Vec<(&str, OciSpec)> = Vec::new();
    let mut s = lab.conformance_spec(&Lab::act(&Act::Wait));
    s.rootfs = Rootfs::Dir(missing.clone());
    cases.push(("rootfs.dir", s));
    let mut s = lab.conformance_spec(&Lab::act(&Act::Wait));
    s.rootfs = Rootfs::Layers(vec![lab.base.clone(), lab.app.join("app/hello")]);
    cases.push(("rootfs.layers[1]", s));
    let mut s = lab.conformance_spec(&Lab::act(&Act::Wait));
    s.mounts.push(bind(&missing.to_string_lossy(), "/missing"));
    cases.push(("mounts[2].source", s));
    if lab.rootless {
        let mut s = lab.conformance_spec(&Lab::act(&Act::Wait));
        s.init.user = Some(User {
            uid: 1000,
            gid: 1000,
            groups: Vec::new(),
        });
        cases.push(("init.user", s));
    }
    for (field, spec) in cases {
        let got = node.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Oci(Box::new(spec)),
            owner: None,
            band: 0,
        });
        assert!(
            matches!(&got, Err(Refusal::Invalid { what: "oci spec", why }) if why.contains(field)),
            "{field}: {got:?}"
        );
        nothing_left(&node, &id, &box_root, &state);
    }
}

/// T-T7.1.2 (goal 5 extended step 5): what a crash leaves of a box with a rendered bundle, the layer
/// already holding a directory of mode 0 and a deep tree, as a container can leave one. Do: as today's
/// node leaves a box with no record yet, leave a bundle with its cgroup (a create killed after
/// rendering it, before the container's state directory); a bundle alone and a container's state
/// directory alone (removals killed after the box's cgroup went, the cgroup removed first); as a node
/// before step 5 left one, a bundle with its container's state directory and a record saying the box
/// was being made; and a file among the bundles, which no box's part is. Expect: recovery removes each
/// box, naming what it found of those with no record, nothing of any is left, and the file is.
fn recovery_removes_a_half_made_bundle(lab: &Lab, own: &Path) {
    let (node, box_root, state) = lab.node(own, "recovery");
    let id = |n: &str| BoxId::new(n).unwrap();
    let (alone, with_cgroup, older) = (id("half-alone"), id("half-cgroup"), id("half-older"));
    let state_only = id("half-state");
    let container = node.container_dir(&older);
    std::fs::create_dir_all(node.container_dir(&state_only)).unwrap();
    drop(node);
    let half_made = |id: &BoxId| {
        let bundle = state.join("bundles").join(id.as_str());
        let mut deep = bundle.join("upper");
        for _ in 0..200 {
            deep.push("d");
        }
        std::fs::create_dir_all(&deep).unwrap();
        std::fs::create_dir_all(bundle.join("work/work")).unwrap();
        std::fs::set_permissions(
            bundle.join("work/work"),
            std::fs::Permissions::from_mode(0o000),
        )
        .unwrap();
        bundle
    };
    for id in [&alone, &with_cgroup] {
        half_made(id);
    }
    std::fs::create_dir(box_root.join(OCI_BOXES).join(with_cgroup.as_str())).unwrap();
    std::fs::create_dir_all(&container).unwrap();
    let spec = BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: older.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Oci(Box::new(lab.conformance_spec(&Lab::act(&Act::Wait)))),
        owner: None,
        band: 0,
    };
    let record = BoxRecord::new(
        spec,
        Phase::Creating,
        vec![container.clone(), half_made(&older)],
    );
    // Where that node kept every record: its state directory.
    std::fs::write(
        state.join(format!("{older}.json")),
        record.encode().unwrap(),
    )
    .unwrap();
    let stray = state.join("bundles/not-a-box");
    std::fs::write(&stray, b"").unwrap();
    let (restarted, _, _) = lab.node(own, "recovery");
    let summary = restarted.recover().expect("recover");
    let why = |id: &BoxId| {
        summary
            .removed
            .iter()
            .find(|(r, _)| r == id)
            .map(|(_, why)| why.clone())
            .unwrap_or_else(|| panic!("{id} not removed: {summary:?}"))
    };
    assert!(
        why(&alone).ends_with(": its bundle left"),
        "{}",
        why(&alone)
    );
    assert!(
        why(&with_cgroup).ends_with(": its cgroup, its bundle left"),
        "{}",
        why(&with_cgroup)
    );
    assert!(
        why(&state_only).ends_with(": its container's state left"),
        "{}",
        why(&state_only)
    );
    assert!(why(&older).contains("being made"), "{}", why(&older));
    assert!(!container.exists(), "the half-made container state is left");
    for id in [&alone, &with_cgroup, &state_only, &older] {
        nothing_left(&restarted, id, &box_root, &state);
    }
    assert!(
        !summary
            .removed
            .iter()
            .any(|(id, _)| id.as_str() == "not-a-box"),
        "{summary:?}"
    );
    assert!(
        stray.exists(),
        "a file that's no box's part is left as it was"
    );
}

/// Deletes its box when dropped, so a check that fails leaves no container running.
struct Deleting<'a>(&'a Node, BoxId);

impl Drop for Deleting<'_> {
    fn drop(&mut self) {
        let _ = self.0.delete(&self.1);
    }
}

/// Nothing of box `id` is left on the node: no bundle, no container state, no cgroup, no record in
/// either place a node keeps them, and no mount anywhere naming its bundle.
fn nothing_left(node: &Node, id: &BoxId, box_root: &Path, state: &Path) {
    let bundle = state.join("bundles").join(id.as_str());
    assert!(!bundle.exists(), "{id}'s bundle is left");
    assert!(
        !node.container_dir(id).exists(),
        "{id}'s container state is left"
    );
    assert!(
        !box_root.join(OCI_BOXES).join(id.as_str()).exists(),
        "{id}'s cgroup is left"
    );
    let runtime = node.oci_root().to_owned();
    for record in [
        state.join(format!("{id}.json")),
        hex_node::store::runtime_record(&records_dir(&runtime, state).unwrap(), id),
    ] {
        assert!(
            !record.exists(),
            "{id}'s record is left at {}",
            record.display()
        );
    }
    assert_eq!(node.inspect(id), Err(Refusal::NoSuchBox(id.clone())));
    let mounts = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
    assert!(
        !mounts.contains(&bundle.to_string_lossy().into_owned()),
        "a mount names {id}'s bundle"
    );
}

/// `sh -c script` in the box; its stdout, once it has exited 0.
fn run(node: &Node, id: &BoxId, script: &str) -> String {
    let (exit, out) = exec(node, id, script);
    assert_eq!(exit, Exit::Code(0), "{script}: {out}");
    out
}

fn exit_of(node: &Node, id: &BoxId, script: &str) -> Exit {
    exec(node, id, script).0
}

fn exec(node: &Node, id: &BoxId, script: &str) -> (Exit, String) {
    let (mut r, w) = std::io::pipe().unwrap();
    let mut proc = node
        .exec(
            id,
            Program::of(&["/bin/sh", "-c", script]),
            Stdio {
                stdout: Some(w),
                ..Stdio::null()
            },
        )
        .expect("exec");
    let mut out = String::new();
    r.read_to_string(&mut out).unwrap();
    let exit = proc
        .wait_for(conformance::PATIENCE)
        .unwrap()
        .expect("the program ends");
    (exit, out)
}

/// One `/proc/<pid>/mountinfo` line (proc_pid_mountinfo(5)).
#[derive(Debug)]
struct MountLine {
    point: String,
    options: String,
    fstype: String,
    super_options: String,
}

fn mountinfo(text: &str) -> Vec<MountLine> {
    text.lines()
        .map(|line| {
            let (left, right) = line.split_once(" - ").unwrap();
            let left: Vec<&str> = left.split(' ').collect();
            let right: Vec<&str> = right.split(' ').collect();
            MountLine {
                point: left[4].to_owned(),
                options: left[5].to_owned(),
                fstype: right[0].to_owned(),
                super_options: right[2].to_owned(),
            }
        })
        .collect()
}

/// Every entry under `dir`, by path: its kind and mode, and a file's bytes or a link's target. Walked
/// with a stack.
/// The creates the node counted a network namespace of the box's own for: (taken from its pool, made
/// on the create's path).
fn boxes_namespaces(node: &Node) -> (u64, u64) {
    let counts = node.netns().unwrap();
    (counts.taken, counts.made_on_path)
}

/// The pool's refill thread's make of a network namespace: the `netns.make` record (hex_testkit::waits).
static POOL_HOLDS: Patience = Patience::seeded(
    "test.oci_spec_box.pool_holds",
    hex_testkit::waits::NETNS_MAKE,
);

/// Waits for the fact a create from the pool needs: a root node's pool holds a namespace, which its
/// thread makes as soon as the node opens.
fn pool_holds_one(node: &Node) {
    hex_testkit::waits::until(&POOL_HOLDS, "a namespace in the node's pool", || {
        let c = node.netns().unwrap();
        c.made_ahead > c.taken + c.discarded
    })
    .unwrap();
}

fn snapshot(dir: &Path) -> BTreeMap<PathBuf, (String, u32, Vec<u8>)> {
    let mut out = BTreeMap::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(d) = todo.pop() {
        for entry in std::fs::read_dir(&d).unwrap() {
            let path = entry.unwrap().path();
            let meta = std::fs::symlink_metadata(&path).unwrap();
            let (kind, bytes) = if meta.is_dir() {
                todo.push(path.clone());
                ("dir", Vec::new())
            } else if meta.file_type().is_symlink() {
                (
                    "link",
                    std::fs::read_link(&path)
                        .unwrap()
                        .into_os_string()
                        .into_encoded_bytes(),
                )
            } else {
                ("file", std::fs::read(&path).unwrap())
            };
            out.insert(
                path.strip_prefix(dir).unwrap().to_path_buf(),
                (kind.to_owned(), meta.permissions().mode(), bytes),
            );
        }
    }
    out
}

/// A layer that's a root of its own: the directories a container's mounts land on, a file and a
/// directory of its own, and the host's top-level links into `/usr` (`/bin`, `/lib64`, …), so the
/// loader and `/bin/sh` resolve, through the `/usr` bind, as on the host.
fn build_base(dir: &Path) {
    for d in ["proc", "dev", "sys", "tmp", "usr", "etc", "app"] {
        std::fs::create_dir_all(dir.join(d)).unwrap();
    }
    std::fs::write(dir.join("base-file"), "from the base layer\n").unwrap();
    for link in ["bin", "sbin", "lib", "lib64"] {
        if let Ok(target) = std::fs::read_link(Path::new("/").join(link)) {
            let target = target.to_string_lossy().trim_start_matches('/').to_owned();
            std::os::unix::fs::symlink(target, dir.join(link)).unwrap();
        }
    }
}

fn usr() -> Mount {
    bind("/usr", "/usr")
}

fn bind(source: &str, destination: &str) -> Mount {
    Mount {
        source: PathBuf::from(source),
        destination: destination.to_owned(),
        read_only: true,
    }
}

/// This process's namespace of `kind`, as `readlink /proc/self/ns/<kind>` prints it.
fn own_ns(kind: &str) -> String {
    std::fs::read_link(format!("/proc/self/ns/{kind}"))
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// Whether this process may make a user namespace: `unshare -U` of a program that does nothing.
fn userns_available() -> bool {
    Command::new("unshare")
        .args(["-U", "true"])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
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
    let cache =
        PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/hex-t7/ocispec");
    std::fs::create_dir_all(&cache).expect("cache dir");
    cache
}
