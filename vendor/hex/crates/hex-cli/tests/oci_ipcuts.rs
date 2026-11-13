//! Goal 5 extended, step 4 (AC-T7.1.4, T-T7.1.2's isolation): a root node's containers join an ipc and a uts
//! namespace made ahead, by `setns`, in place of their init's clone making them. "Do X, expect Y" (CLAUDE.md §6):
//! - HEX-IPCUTS-3, as root: two boxes from the pool and a box on a node with the pool drained (bound 0). Each box's
//!   namespaces are its own, never another box's nor the node's; the drained create works and is counted dry; a pair
//!   taken is never handed out again, over more boxes than the pool's bound, all alive at once, and no process keeps
//!   a descriptor of one.
//! - HEX-IPCUTS-4, as root: a box writes a SysV shared-memory segment, makes a POSIX message queue and has its hostname
//!   set; a second box from the pool looks for all three and sees none. The queue also shows the init joined before it
//!   mounted `/dev/mqueue`, whose filesystem belongs to the mounter's ipc namespace.
//! - HEX-IPCUTS-5: a container in a user namespace of its own takes no pair and is counted as made on the path. That
//!   its `/dev/mqueue` mounts shows its ipc namespace is its user namespace's: mounting needs `CAP_SYS_ADMIN` in the
//!   user namespace that owns the ipc namespace, which a pair the node made would refuse it, as `setns` would.
//! - HEX-IPCUTS-6, rootless: a node makes none, a bound above 0 is refused, and its container is counted on the path.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::collections::BTreeSet;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;

use hex_api::{Bounds, BoxId, BoxSpec, ClassSpec, Exit, Program, Refusal, Runtime, Stdio};
use hex_core::patience::Patience;
use hex_node::{Node, OCI_BOXES};
use hex_testkit::waits::until;

const TEST: &str = "a_root_nodes_boxes_join_ipc_and_uts_namespaces_made_ahead";
/// The ipc and uts pool's refill thread's makes, which no recorded operation makes and no census row times yet: measuring,
/// for the fact alone, each answer recorded for the census to come (hex_testkit::waits; the lead's rule of 2026-11-08).
static PAIRS: Patience = Patience::measuring("test.oci_ipcuts.pairs");
const PATH: &str = "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

#[test]
fn a_root_nodes_boxes_join_ipc_and_uts_namespaces_made_ahead() {
    let exe = env!("CARGO_BIN_EXE_hex").to_owned();
    let cache = cache_dir();
    let own = match prepare_cgroup(&cache) {
        Ok(own) => own,
        // A shared cgroup can't hand its controllers down: the test runs itself again in a scope of its own, as
        // oci_box.rs does, so it doesn't pass by skipping.
        Err(direct) if std::env::var_os("HEX_DELEGATED").is_none() => {
            let has_systemd = Command::new("systemd-run")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !has_systemd {
                eprintln!(
                    "SKIP {TEST}: own cgroup isn't ours to subdivide ({direct}) and no systemd --user"
                );
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
            return;
        }
        // HEX_DELEGATED promised this binary a cgroup delegated to it: a preparation that fails there fails the test,
        // never a skip that would hide the breakage.
        Err(why) => panic!("its cgroup isn't delegated to it ({why})"),
    };
    let lab = Lab::new(&cache, &exe);
    if host_id("-u") != 0 {
        eprintln!(
            "oci_ipcuts rootless: {}",
            a_rootless_node_makes_none(&lab, &own)
        );
        return;
    }
    // Each case says what it saw, so a lab's record shows every one ran.
    for (case, saw) in [
        (
            "from the pool",
            boxes_from_the_pool_and_one_drained(&lab, &own),
        ),
        (
            "isolation",
            a_second_box_sees_nothing_of_the_first(&lab, &own),
        ),
        (
            "user namespace",
            a_user_namespace_of_its_own_keeps_the_clones_flags(&lab, &own),
        ),
    ] {
        eprintln!("oci_ipcuts {case}: {saw}");
    }
}

/// HEX-IPCUTS-3 (the module's first case).
fn boxes_from_the_pool_and_one_drained(lab: &Lab, own: &Path) -> String {
    let node_ns = own_namespaces();
    let state = lab.state();
    let root = own.join("hex-ipcuts-dry");
    let drained = lab
        .node(&root, state.path())
        .with_ipc_uts_bound(0)
        .expect("a drained pool");
    let dry = make(&drained, "ipcuts-dry", lab.bundle(None, false));
    let dry_ns = namespaces_in(&drained, &dry);
    let c = drained.ipc_uts().unwrap();
    assert_eq!(
        (c.made_ahead, c.taken, c.made_on_path),
        (0, 0, 1),
        "the drained create is counted dry: (made ahead, taken, made on the path)"
    );

    let state = lab.state();
    let pooled_root = own.join("hex-ipcuts");
    let node = lab
        .node(&pooled_root, state.path())
        .with_ipc_uts_bound(2)
        .expect("a pool of two");
    let mut seen: BTreeSet<String> = node_ns.iter().chain(dry_ns.iter()).cloned().collect();
    assert_eq!(seen.len(), 4, "the drained box's are its own: {seen:?}");
    let mut boxes = Vec::new();
    // Past the pool's bound, each box alive while the next is made, so a pair handed out twice would show here.
    for k in 0..8 {
        wait_for(
            &PAIRS,
            || {
                let c = node.ipc_uts().unwrap();
                c.made_ahead - c.taken - c.discarded >= 1
            },
            "the pool to hold a pair",
        );
        let id = make(&node, &format!("ipcuts-{k}"), lab.bundle(None, false));
        for ns in namespaces_in(&node, &id) {
            assert!(
                !held_by_any_process(&ns),
                "no process keeps {ns}'s descriptor"
            );
            assert!(
                seen.insert(ns.clone()),
                "box {k}'s {ns} is no other's: {seen:?}"
            );
        }
        boxes.push(id);
    }
    let c = node.ipc_uts().unwrap();
    assert_eq!(
        (c.taken, c.made_on_path, c.failed),
        (8, 0, 0),
        "every box took a pair: (taken, made on the path, failed)"
    );
    for id in &boxes {
        node.delete(id).expect("delete");
    }
    drained.delete(&dry).expect("delete");
    drop((node, drained));
    sweep(&pooled_root);
    sweep(&root);
    format!(
        "8 boxes of {} namespaces, none shared, 1 drained",
        seen.len() - 4
    )
}

/// HEX-IPCUTS-4 (the module's second case).
fn a_second_box_sees_nothing_of_the_first(lab: &Lab, own: &Path) -> String {
    let state = lab.state();
    let root = own.join("hex-ipcuts-iso");
    let node = lab
        .node(&root, state.path())
        .with_ipc_uts_bound(2)
        .expect("a pool of two");
    wait_for(
        &PAIRS,
        || node.ipc_uts().unwrap().made_ahead >= 2,
        "the pool to hold two pairs",
    );
    let writer = make(
        &node,
        "ipcuts-writer",
        lab.bundle(Some("hex-ipcuts-writer"), false),
    );
    run_in(&node, &writer, &["/usr/bin/ipcmk", "-M", "4096"]);
    run_in(
        &node,
        &writer,
        &["/usr/bin/touch", "/dev/mqueue/hex-writer"],
    );
    let segments = run_in(&node, &writer, &["/usr/bin/cat", "/proc/sysvipc/shm"]);
    assert_eq!(
        segments.lines().count(),
        2,
        "the writer's segment, under the file's header: {segments}"
    );
    assert_eq!(
        run_in(
            &node,
            &writer,
            &["/usr/bin/cat", "/proc/sys/kernel/hostname"]
        )
        .trim(),
        "hex-ipcuts-writer",
        "the writer's hostname"
    );
    let key = segments
        .lines()
        .nth(1)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned();
    let on_node = std::fs::read_to_string("/proc/sysvipc/shm").unwrap();
    assert!(
        !on_node
            .lines()
            .skip(1)
            .any(|l| l.split_whitespace().next() == Some(key.as_str())),
        "the node's ipc namespace holds no segment of the box's: {on_node}"
    );
    let node_name = std::fs::read_to_string("/proc/sys/kernel/hostname").unwrap();
    assert_ne!(
        node_name.trim(),
        "hex-ipcuts-writer",
        "the node's hostname is its own"
    );

    let looker = make(&node, "ipcuts-looker", lab.bundle(None, false));
    let c = node.ipc_uts().unwrap();
    assert_eq!(
        (c.taken, c.made_on_path),
        (2, 0),
        "both boxes came from the pool"
    );
    let segments = run_in(&node, &looker, &["/usr/bin/cat", "/proc/sysvipc/shm"]);
    assert_eq!(
        segments.lines().count(),
        1,
        "the second box sees no segment: {segments}"
    );
    assert_eq!(
        run_in(&node, &looker, &["/usr/bin/ls", "-A", "/dev/mqueue"]),
        "",
        "the second box sees no queue"
    );
    assert_eq!(
        run_in(
            &node,
            &looker,
            &["/usr/bin/cat", "/proc/sys/kernel/hostname"]
        ),
        node_name,
        "the second box has the name a fresh namespace starts with, the node's"
    );
    node.delete(&writer).expect("delete");
    node.delete(&looker).expect("delete");
    drop(node);
    sweep(&root);
    "the writer's segment, queue and hostname unseen by the second box and the node".to_owned()
}

/// HEX-IPCUTS-5 (the module's third case).
fn a_user_namespace_of_its_own_keeps_the_clones_flags(lab: &Lab, own: &Path) -> String {
    let node_ns = own_namespaces();
    let state = lab.state();
    let root = own.join("hex-ipcuts-user");
    let node = lab
        .node(&root, state.path())
        .with_ipc_uts_bound(2)
        .expect("a pool of two");
    wait_for(
        &PAIRS,
        || node.ipc_uts().unwrap().made_ahead >= 2,
        "the pool to hold two pairs",
    );
    let id = make(&node, "ipcuts-user", lab.bundle(None, true));
    for ns in namespaces_in(&node, &id) {
        assert!(!node_ns.contains(&ns), "{ns} is the box's own");
    }
    let c = node.ipc_uts().unwrap();
    assert_eq!(
        (c.taken, c.made_on_path),
        (0, 1),
        "(taken, made on the path)"
    );
    node.delete(&id).expect("delete");
    drop(node);
    sweep(&root);
    "no pair taken, counted on the path, its /dev/mqueue mounted in its own".to_owned()
}

/// HEX-IPCUTS-6 (the module's last case).
fn a_rootless_node_makes_none(lab: &Lab, own: &Path) -> String {
    let state = lab.state();
    let root = own.join("hex-ipcuts-rootless");
    let refused = lab.node(&root, state.path()).with_ipc_uts_bound(1);
    assert!(
        matches!(refused, Err(Refusal::Invalid { .. })),
        "a rootless node refuses a bound above 0"
    );
    let node = lab.node(&root, state.path());
    let id = make(&node, "ipcuts-rootless", lab.bundle(None, true));
    let c = node.ipc_uts().unwrap();
    assert_eq!(
        (c.made_ahead, c.taken, c.made_on_path),
        (0, 0, 1),
        "(made ahead, taken, made on the path)"
    );
    node.delete(&id).expect("delete");
    drop(node);
    sweep(&root);
    "a bound above 0 refused, its container counted on the path".to_owned()
}

fn sweep(box_root: &Path) {
    let _ = std::fs::remove_dir(box_root.join(OCI_BOXES));
    let _ = std::fs::remove_dir(box_root);
}

/// Waits for `fact` on `wait`. The node publishes no event as its pool fills; its counts are read again once other
/// threads have run, as oci_box.rs waits out the inits' refill, rather than after a picked sleep (hex_testkit::waits).
fn wait_for(wait: &Patience, fact: impl FnMut() -> bool, what: &str) {
    until(wait, what, fact).unwrap();
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
        band: 0,
    })
    .unwrap_or_else(|e| panic!("create {id}: {e}"));
    node.start(&id).unwrap();
    id
}

/// What `args` writes to its standard output, exec'd into box `id`, which it must exit 0 from.
fn run_in(node: &Node, id: &BoxId, args: &[&str]) -> String {
    let (mut out, w) = std::io::pipe().unwrap();
    let mut proc = node
        .exec(
            id,
            Program::of(args),
            Stdio {
                stdout: Some(w),
                ..Stdio::null()
            },
        )
        .unwrap_or_else(|e| panic!("exec {args:?}: {e}"));
    let mut text = String::new();
    out.read_to_string(&mut text).unwrap();
    assert_eq!(
        hex_api::conformance::end_of(&mut proc).unwrap(),
        Exit::Code(0),
        "{args:?} in {id}: {text}"
    );
    text
}

/// Box `id`'s ipc and uts namespaces, as a program in it reads them.
fn namespaces_in(node: &Node, id: &BoxId) -> [String; 2] {
    ["ipc", "uts"].map(|kind| {
        run_in(
            node,
            id,
            &["/usr/bin/readlink", &format!("/proc/self/ns/{kind}")],
        )
        .trim()
        .to_owned()
    })
}

fn own_namespaces() -> [String; 2] {
    ["ipc", "uts"].map(|kind| {
        std::fs::read_link(format!("/proc/self/ns/{kind}"))
            .unwrap()
            .to_string_lossy()
            .into_owned()
    })
}

/// Whether any process this one may read holds a descriptor of namespace `ns`, as `ipc:[4026532000]`.
fn held_by_any_process(ns: &str) -> bool {
    let Ok(procs) = std::fs::read_dir("/proc") else {
        return false;
    };
    procs.flatten().any(|p| {
        // A process gone since the listing holds nothing, and a non-process entry has no fd directory.
        std::fs::read_dir(p.path().join("fd")).is_ok_and(|fds| {
            fds.flatten()
                .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|l| l.to_string_lossy() == ns))
        })
    })
}

/// Where one run keeps its bundles and the rootfs they share.
struct Lab {
    exe: String,
    rootfs: PathBuf,
    dir: tempfile::TempDir,
    uid: u32,
    gid: u32,
}

impl Lab {
    fn new(cache: &Path, exe: &str) -> Self {
        let dir = tempfile::Builder::new()
            .prefix("ociipcuts-")
            .tempdir_in(cache)
            .unwrap();
        let rootfs = dir.path().join("rootfs");
        build_rootfs(&rootfs);
        Self {
            exe: exe.to_owned(),
            rootfs,
            dir,
            uid: host_id("-u"),
            gid: host_id("-g"),
        }
    }

    fn state(&self) -> tempfile::TempDir {
        tempfile::tempdir_in(self.dir.path()).unwrap()
    }

    fn node(&self, box_root: &Path, state: &Path) -> Node {
        Node::open(box_root, state, state)
            .unwrap()
            .with_oci_helper(PathBuf::from(&self.exe))
    }

    /// A bundle whose init waits, with ipc and uts namespaces of its own and `/dev/mqueue` mounted, `hostname` its
    /// hostname if given, and, with `user_ns`, a user namespace of its own that maps this process's ids to root.
    fn bundle(&self, hostname: Option<&str>, user_ns: bool) -> PathBuf {
        let dir = tempfile::Builder::new()
            .prefix("bundle-")
            .tempdir_in(self.dir.path())
            .unwrap()
            .keep();
        let hostname = hostname.map_or(String::new(), |h| format!(r#""hostname": "{h}","#));
        // A box in a user namespace of its own on the host's network names Docker's default capability
        // sets (`hex_core::bundle`), without CAP_SYS_ADMIN, which spec.rs refuses it (b1c8e99).
        let (user, mappings, caps) = if user_ns {
            let set = format!("{:?}", hex_core::bundle::CAPABILITIES);
            (
                r#"{"type": "user"}, "#.to_owned(),
                format!(
                    r#", "uidMappings": [{{"containerID": 0, "hostID": {}, "size": 1}}],
                       "gidMappings": [{{"containerID": 0, "hostID": {}, "size": 1}}]"#,
                    self.uid, self.gid
                ),
                format!(
                    r#""capabilities": {{"bounding": {set}, "effective": {set}, "permitted": {set}}}, "#
                ),
            )
        } else {
            (String::new(), String::new(), String::new())
        };
        let config = format!(
            r#"{{
  "ociVersion": "1.3.0",
  {hostname}
  "root": {{"path": "{rootfs}", "readonly": false}},
  "process": {{"cwd": "/", "args": ["/hex", "selftest-box", "wait"], "env": ["{PATH}"],
              "user": {{"uid": 0, "gid": 0}}, {caps}"noNewPrivileges": true}},
  "mounts": [
    {{"destination": "/proc", "type": "proc", "source": "proc"}},
    {{"destination": "/dev", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid","mode=755","size=65536k"]}},
    {{"destination": "/dev/mqueue", "type": "mqueue", "source": "mqueue", "options": ["nosuid","noexec","nodev"]}},
    {{"destination": "/tmp", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid","nodev"]}},
    {{"destination": "/usr", "type": "bind", "source": "/usr", "options": ["rbind","ro"]}},
    {{"destination": "/hex", "type": "bind", "source": "{exe}", "options": ["bind","ro"]}}
  ],
  "linux": {{
    "namespaces": [{user}{{"type": "pid"}}, {{"type": "mount"}}, {{"type": "ipc"}}, {{"type": "uts"}}]{mappings}
  }}
}}"#,
            rootfs = self.rootfs.display(),
            exe = self.exe,
        );
        std::fs::write(dir.join("config.json"), config).unwrap();
        dir
    }
}

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
    let cache = PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/hex-t7/ocibox");
    std::fs::create_dir_all(&cache).expect("cache dir");
    cache
}
