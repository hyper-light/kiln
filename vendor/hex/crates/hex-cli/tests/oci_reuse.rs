//! Goal 5 extended, step 6 (decision 5, "The cgroup lock"; docs/research/runtime/goal5-cgroup-lock.md,
//! f19-f50, f61-f65, f81, f82): fewer holds of the kernel's host-wide `cgroup_mutex` per box, through the
//! `Runtime` seam (`hex_node::Node`). "do X, expect Y" (CLAUDE.md §6), each case naming the plan step's
//! test it serves:
//! - test 1, how a delete ends a box: an `oci` box whose init made a pid namespace of its own, a program
//!   exec'd in it, is deleted: expect it ended through the init's pidfd, with no write to its
//!   `cgroup.kill` (an inotify watch on the file sees none), its cgroup gone and the program reaped, its
//!   end read as SIGKILL. A box with a process from outside its namespace moved into its cgroup: expect
//!   the delete to fall back to `cgroup.kill` (the watch sees the write), counted, and the outsider
//!   killed. A `process` box: expect `cgroup.kill` (the watch sees it), counted. A box whose init ended
//!   before its delete, read first, and enough of one owner whose inits end at once (as many as catch the old
//!   delete's measured kill rate with 99% odds), each deleted right after its start from 32 threads: expect
//!   each ended through its init, none by `cgroup.kill`;
//! - test 2, a cgroup reused within one owner: box A of owner O burns CPU, holds 64 MiB, has a program
//!   OOM-killed and forks refused, and is deleted; box B of owner O is created: expect B in A's slot, B's
//!   reported CPU under 1% of A's, its peak under A's and its event counts its own (0) while the cgroup's
//!   own files still hold A's, and every bound B sets in force, every one it doesn't at its default.
//!   With it, `cpu.max` (the lead's ruling, 2026-11-03T01:19Z): B of O bounded otherwise than A, then C of
//!   O bounded as B: expect the file written for B and left for C, holding their values each time;
//! - test 3, each guard: after A of owner O is deleted, a sub-cgroup made in its cgroup, its
//!   `memory.oom.group` set, a live process moved in, or 64 MiB left charged by A: expect B of O a fresh
//!   cgroup, A's removed and the refusal counted by its reason. A killed through `cgroup.kill` (a signal
//!   KILL to the box) and deleted: expect its cgroup removed, not kept, counted, and B of O a fresh one
//!   that runs: Linux 6.19 kills every task later cloned into a cgroup `cgroup.kill` was written to
//!   (its `kill_seq`, kernel/cgroup/cgroup.c:6702-6705 and :6941). B of another owner: a fresh cgroup,
//!   counted, and A's kept in O's pool, then handed to O's next box (Ada's call, 2026-11-03: keep, not
//!   the plan's "A's is removed"). A of no owner, holding CAP_SYS_ADMIN, or whose spec
//!   gives no capability sets (it keeps its creator's, all of them): a cgroup of its own, removed with
//!   it, counted; B holding CAP_SYS_ADMIN beside its owner's idle cgroup: a cgroup of its own, counted,
//!   the idle one left;
//! - the network namespace, reused within one owner (Ada's call, 2026-11-03; decision 5), root only: A of
//!   O, which could change nothing of its namespace, deleted; B of O: expect A's namespace, counted, its
//!   loopback up and A's counters still in it. A's namespace left holding a TIME_WAIT connection, an open
//!   socket, an address or a route: expect B fresh and the refusal counted by its reason, the open socket's
//!   check counted as reading the protocols; one holding a socket never hashed: expect the protocols read,
//!   counted, and the namespace kept. A holding
//!   CAP_NET_ADMIN, with `/proc/sys` writable, or of no owner: never kept, counted. Boxes of another owner P
//!   beside O's idle namespace: each fresh, counted, never O's; then O's next box gets O's and P's next box
//!   P's oldest. B of O given a namespace the node checked: expect B to see the node's netlink socket in it and
//!   its send to that socket refused, its send to the kernel gone, the socket's queue and drops unchanged, and
//!   the namespace kept again at its next check;
//! - a parked create's wakes (the lead's (A) of 2026-11-09, decision 8's claim): three creates of one owner, each
//!   claiming the init its last box's delete parked: expect the creating thread to sleep, in the middle one, once for
//!   each word the init says on its sync pipe (its ready, and rootless its ask for its maps before it), and for no
//!   answer to the claim.
//!
//! Rootless where the caller isn't root, in a delegated cgroup v2 subtree, as oci_box.rs; where neither
//! is possible it prints SKIP and why rather than pass falsely.
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
use std::mem::MaybeUninit;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use hex_api::conformance::{Act, PATIENCE};
use hex_api::{Bounds, BoxId, BoxOwner, BoxSpec, ClassSpec, Exit, Program, Runtime, Stdio};
use hex_core::netns_reuse::Refused as NetRefused;
use hex_core::patience::Patience;
use hex_core::reuse::Refused;
use hex_node::Node;
use hex_testkit::waits::{DELETE, EMPTYING, PROGRAM, until};
use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fs::inotify;
use rustix::process::{Pid, PidfdFlags};

const TEST: &str = "a_boxs_cgroup_is_ended_without_the_lock_and_reused_within_its_owner";
/// Set in the copy of this binary that runs [`a_node_past_its_files_bound_checks_by_entering_counted`] alone: it lowers
/// its hard descriptor limit, which no process raises back without CAP_SYS_RESOURCE, so no later case runs under it.
const FILES_BOUND_ENV: &str = "HEX_OCI_REUSE_FILES_BOUND";
/// What the copy says once the case has passed.
const FILES_BOUND_PASS: &str = "netns: past the files' bound, checks entered, counted";

/// Docker's default capability set (moby's `oci/caps/defaults.go`, as crates/hex-core/src/bundle.rs
/// renders it): no CAP_SYS_ADMIN, so a box holding these can't change its cgroup.
const DOCKER_CAPS: &str = r#"["CAP_CHOWN","CAP_DAC_OVERRIDE","CAP_FSETID","CAP_FOWNER","CAP_MKNOD","CAP_NET_RAW","CAP_SETGID","CAP_SETUID","CAP_SETFCAP","CAP_SETPCAP","CAP_NET_BIND_SERVICE","CAP_SYS_CHROOT","CAP_KILL","CAP_AUDIT_WRITE"]"#;

#[test]
fn a_boxs_cgroup_is_ended_without_the_lock_and_reused_within_its_owner() {
    let exe = env!("CARGO_BIN_EXE_hex").to_owned();
    let cache = cache_dir();
    if std::env::var_os(FILES_BOUND_ENV).is_some() {
        let own = prepare_cgroup(&cache).expect("this copy's own delegated cgroup");
        a_node_past_its_files_bound_checks_by_entering_counted(&Lab::new(&cache, &exe), &own);
        return;
    }
    match prepare_cgroup(&cache) {
        Ok(own) => exercise(&own, &cache, &exe),
        Err(direct) if std::env::var_os("HEX_DELEGATED").is_none() => {
            let has_systemd = Command::new("systemd-run")
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !has_systemd {
                eprintln!(
                    "SKIP: own cgroup isn't ours to subdivide ({direct}) and no systemd --user for a private scope"
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
        }
        Err(direct) => {
            eprintln!("SKIP: no usable cgroup v2 delegation ({direct}); run on a cgroup-v2 host")
        }
    }
}

/// What a box's process may hold.
#[derive(Clone, Copy)]
enum Caps {
    /// Docker's defaults, given in every set.
    Docker,
    /// Docker's defaults and CAP_SYS_ADMIN.
    SysAdmin,
    /// No capability sets given, so the process keeps its creator's: all of them.
    Unset,
    /// Docker's defaults and CAP_NET_ADMIN.
    NetAdmin,
}

/// Where one run keeps its bundles and box roots.
struct Lab {
    exe: String,
    rootfs: PathBuf,
    bundles: tempfile::TempDir,
    rootless: bool,
    uid: u32,
    gid: u32,
}

impl Lab {
    fn new(cache: &Path, exe: &str) -> Self {
        let (uid, gid) = (host_id("-u"), host_id("-g"));
        let bundles = tempfile::Builder::new()
            .prefix("ocireuse-")
            .tempdir_in(cache)
            .expect("bundles dir");
        let rootfs = bundles.path().join("rootfs");
        build_rootfs(&rootfs);
        Self {
            exe: exe.to_owned(),
            rootfs,
            bundles,
            rootless: uid != 0,
            uid,
            gid,
        }
    }

    /// A bundle whose init waits, its process holding `caps`.
    fn bundle(&self, caps: Caps) -> PathBuf {
        self.bundle_running(caps, &["/hex", "selftest-box", "wait"])
    }

    /// A bundle whose init runs `args`, its process holding `caps`.
    fn bundle_running(&self, caps: Caps, args: &[&str]) -> PathBuf {
        self.bundle_with(caps, args, false, false)
    }

    /// A bundle whose init runs `args`, its process holding `caps`, with a network namespace of its own
    /// when `network` and `/proc/sys` read-only when `readonly_sys`.
    fn bundle_with(&self, caps: Caps, args: &[&str], network: bool, readonly_sys: bool) -> PathBuf {
        let dir = tempfile::Builder::new()
            .prefix("bundle-")
            .tempdir_in(self.bundles.path())
            .expect("bundle dir")
            .keep();
        let (namespaces, mappings) = if self.rootless {
            (
                r#"{"type":"user"},{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"}"#
                    .to_owned(),
                format!(
                    r#","uidMappings":[{{"containerID":0,"hostID":{},"size":1}}],"gidMappings":[{{"containerID":0,"hostID":{},"size":1}}]"#,
                    self.uid, self.gid
                ),
            )
        } else {
            (
                r#"{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"}"#.to_owned(),
                String::new(),
            )
        };
        let capabilities = match caps {
            Caps::Docker => Some(DOCKER_CAPS.to_owned()),
            Caps::SysAdmin => Some(DOCKER_CAPS.replace(']', r#","CAP_SYS_ADMIN"]"#)),
            Caps::NetAdmin => Some(DOCKER_CAPS.replace(']', r#","CAP_NET_ADMIN"]"#)),
            Caps::Unset => None,
        }
        .map(|set| {
            format!(
                r#""capabilities": {{"bounding": {set}, "effective": {set}, "permitted": {set}}},
    "#
            )
        })
        .unwrap_or_default();
        // A container in a user namespace holding CAP_SYS_ADMIN needs a network namespace of its own,
        // or its create is refused (beda8a9); one given no capability sets holds it too.
        let namespaces = match caps {
            Caps::SysAdmin | Caps::Unset => format!(r#"{namespaces},{{"type":"network"}}"#),
            Caps::Docker | Caps::NetAdmin if network => {
                format!(r#"{namespaces},{{"type":"network"}}"#)
            }
            Caps::Docker | Caps::NetAdmin => namespaces,
        };
        let readonly = if readonly_sys {
            r#","readonlyPaths": ["/proc/sys"]"#
        } else {
            ""
        };
        let config = format!(
            r#"{{
  "ociVersion": "1.3.0",
  "root": {{"path": "{rootfs}", "readonly": false}},
  "process": {{
    "cwd": "/",
    "args": {args:?},
    "env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
    "user": {{"uid": 0, "gid": 0}},
    {capabilities}"noNewPrivileges": true
  }},
  "mounts": [
    {{"destination": "/proc", "type": "proc", "source": "proc"}},
    {{"destination": "/dev", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid","mode=755","size=65536k"]}},
    {{"destination": "/tmp", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid","nodev"]}},
    {{"destination": "/usr", "type": "bind", "source": "/usr", "options": ["rbind","ro"]}},
    {{"destination": "/hex", "type": "bind", "source": "{exe}", "options": ["bind","ro"]}}
  ],
  "linux": {{"namespaces": [{namespaces}]{mappings}{readonly}}}
}}"#,
            rootfs = self.rootfs.display(),
            exe = self.exe,
        );
        std::fs::write(dir.join("config.json"), config).expect("config.json");
        dir
    }

    /// An `oci` box of `owner` (none when `None`) whose init waits, holding `caps`, bounded by `bounds`.
    fn oci(&self, id: &str, owner: Option<&BoxOwner>, caps: Caps, bounds: Bounds) -> BoxSpec {
        BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: BoxId::new(id).unwrap(),
            bounds,
            class: ClassSpec::Bundle(self.bundle(caps)),
            owner: owner.cloned(),
        }
    }

    fn node(&self, box_root: &Path) -> (Node, tempfile::TempDir) {
        let state = tempfile::tempdir_in(self.bundles.path()).unwrap();
        let node = Node::open(box_root, state.path(), state.path())
            .expect("open a node")
            .with_oci_helper(PathBuf::from(&self.exe));
        (node, state)
    }
}

/// The `hex selftest-box` program for `act`, as the container sees the binary (`/hex`).
fn selftest(act: &Act) -> Program {
    let mut args = vec!["/hex", "selftest-box"];
    let n;
    match act {
        Act::Wait => args.push("wait"),
        Act::Hog(bytes) => {
            n = bytes.to_string();
            args.extend(["hog", n.as_str()]);
        }
        Act::Spawn(count) => {
            n = count.to_string();
            args.extend(["spawn", n.as_str()]);
        }
        _ => panic!("no such act here"),
    }
    Program::of(&args)
}

/// A process's pidfd, which reads ready once it has ended, whoever reaps it: the node agent in this
/// process sweeps up ended processes of its boxes, so a test's own child may be reaped before the test
/// waits for it.
fn pidfd(pid: u32) -> OwnedFd {
    rustix::process::pidfd_open(
        Pid::from_raw(i32::try_from(pid).unwrap()).unwrap(),
        PidfdFlags::empty(),
    )
    .expect("pidfd_open")
}

/// Whether the process `fd` names has ended.
fn has_ended(fd: &OwnedFd) -> bool {
    let mut fds = [PollFd::new(fd, PollFlags::IN)];
    let now = Timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    rustix::event::poll(&mut fds, Some(&now)).unwrap() > 0
}

/// Runs `program` in box `id` to its end, waiting as `wait`, its end's own record, says: for at most its deadline, or,
/// while it measures, for the end alone. Its answer is told to `wait` and to the census as [`until`]'s are.
fn run(node: &Node, id: &BoxId, program: Program, wait: &Patience) -> Exit {
    let mut p = node.exec(id, program, Stdio::null()).expect("exec");
    let began = Instant::now();
    let exit = p
        .wait_for(wait.deadline().unwrap_or(Duration::MAX))
        .expect("wait")
        .expect("the program ended in time");
    hex_testkit::waits::answered(wait, began.elapsed());
    exit
}

fn exercise(own: &Path, cache: &Path, exe: &str) {
    let lab = Lab::new(cache, exe);
    // Whether a container runs here at all (rootless needs unprivileged user namespaces).
    {
        let (node, _state) = lab.node(&own.join("hex-reuse-probe"));
        let spec = lab.oci("probe", None, Caps::Docker, Bounds::default());
        if let Err(e) = node.create(spec) {
            eprintln!("SKIP: rootless OCI unavailable here ({e})");
            return;
        }
        node.delete(&BoxId::new("probe").unwrap()).unwrap();
    }
    a_parked_create_wakes_its_agent_once_a_word(&lab, own);
    a_delete_ends_a_box_through_its_inits_pidfd(&lab, own);
    a_box_of_one_owner_is_given_the_cgroup_its_earlier_box_left(&lab, own);
    an_owners_next_box_is_made_by_the_init_parked_in_its_idle_cgroup(&lab, own);
    a_parked_init_made_with_other_namespaces_is_ended_and_counted(&lab, own);
    parked_procs_stop_at_their_bound_and_none_drops_inline(&lab, own);
    parked_procs_are_a_root_nodes_and_a_rootless_node_makes_none(&lab, own);
    a_box_of_no_owner_is_made_by_an_init_parked_in_a_cgroup_made_ahead(&lab, own);
    a_box_of_no_owner_made_ahead_looks_up_nothing_in_cgroupfs(&lab, own);
    a_box_of_no_owner_made_ahead_is_given_its_own_bounds_alone(&lab, own);
    a_box_of_no_owner_made_ahead_reads_none_of_its_counters(&lab, own);
    a_reused_cgroups_cpu_max_is_written_only_when_its_values_differ(&lab, own);
    each_guard_gives_a_fresh_cgroup_and_counts_why(&lab, own);
    a_boxs_network_namespace_is_reused_within_its_owner_when_clean(&lab, own);
    a_box_can_not_send_to_the_socket_the_check_keeps_in_its_namespace(&lab, own);
    a_kept_namespace_holds_its_proc_net_files_and_they_close_with_it(&lab, own);
    the_files_bound_case_in_a_copy(&lab, own);
    // Last: the node it abandons leaves its seeds running in this process.
    recovery_removes_an_init_parked_by_a_node_that_crashed(&lab, own);
}

/// A watch on a cgroup's directory for writes to one of its files (`cgroup.kill`, `cpu.max`): every
/// write to a file in a watched directory is reported with the file's name (vfs_write's
/// fsnotify_modify; inotify(7), IN_MODIFY), so the test knows whether the node wrote it independently of
/// the node's own counts. The directory is watched since `cgroup.kill` itself is write-only, and
/// inotify_add_watch(2) needs read access to what it watches.
/// Where this user has no inotify instance left (EMFILE: `fs.inotify.max_user_instances` is per user, and a
/// root lab's root shares the host's root, whose services hold them), it says so on stderr and checks
/// nothing: the node's own counts and the file's contents are then the case's evidence, and the rootless
/// runs keep this check.
struct WriteWatch {
    fd: Option<OwnedFd>,
    file: &'static str,
}

impl WriteWatch {
    fn on(cgroup: &Path, file: &'static str) -> Self {
        let fd = match inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)
        {
            Ok(fd) => fd,
            Err(rustix::io::Errno::MFILE) => {
                eprintln!(
                    "NOTE: no inotify instance left for this user here (EMFILE): {file} writes go unwatched, the node's counts stand alone"
                );
                return Self { fd: None, file };
            }
            Err(e) => panic!("inotify: {e}"),
        };
        inotify::add_watch(&fd, cgroup, inotify::WatchFlags::MODIFY)
            .expect("watch the cgroup's directory");
        Self { fd: Some(fd), file }
    }

    /// Whether the file was written since the watch began; `None` where nothing watched it.
    fn written(&self) -> Option<bool> {
        let fd = self.fd.as_ref()?;
        let mut buf = [MaybeUninit::<u8>::uninit(); 4096];
        let mut events = inotify::Reader::new(fd, &mut buf);
        loop {
            match events.next() {
                Ok(e)
                    if e.events().contains(inotify::ReadFlags::MODIFY)
                        && e.file_name()
                            .is_some_and(|n| n.to_bytes() == self.file.as_bytes()) =>
                {
                    return Some(true);
                }
                Ok(_) => {}
                Err(rustix::io::Errno::AGAIN) => return Some(false),
                Err(e) => panic!("reading the cgroup's watch: {e}"),
            }
        }
    }

    /// Every file in the cgroup's directory written since the watch began, by name; `None` where nothing watched it.
    fn files_written(&self) -> Option<BTreeSet<String>> {
        let fd = self.fd.as_ref()?;
        let mut buf = [MaybeUninit::<u8>::uninit(); 4096];
        let mut events = inotify::Reader::new(fd, &mut buf);
        let mut written = BTreeSet::new();
        loop {
            match events.next() {
                Ok(e) if e.events().contains(inotify::ReadFlags::MODIFY) => {
                    if let Some(name) = e.file_name() {
                        written.insert(name.to_string_lossy().into_owned());
                    }
                }
                Ok(_) => {}
                Err(rustix::io::Errno::AGAIN) => return Some(written),
                Err(e) => panic!("reading the cgroup's watch: {e}"),
            }
        }
    }
}

/// Test 1. Do: delete an `oci` box whose init has a pid namespace of its own, with a program exec'd in
/// it; delete one with an outside process moved into its cgroup; delete a `process` box running a
/// program. Expect: the first ended through its init's pidfd with no `cgroup.kill` write, its cgroup gone
/// and its program reaped, ended by SIGKILL; the second falls back to `cgroup.kill`, counted, its
/// outsider killed; the third ends by `cgroup.kill`, counted.
fn a_delete_ends_a_box_through_its_inits_pidfd(lab: &Lab, own: &Path) {
    let (node, _state) = lab.node(&own.join("hex-reuse-end"));
    // The first: a program exec'd in it, still running at the delete.
    let id = BoxId::new("ends").unwrap();
    node.create(lab.oci("ends", None, Caps::Docker, Bounds::default()))
        .unwrap();
    node.start(&id).unwrap();
    let mut program = node.exec(&id, selftest(&Act::Wait), Stdio::null()).unwrap();
    let pid = program.pid();
    let cg = node.cgroup_dir(&id).expect("the box's cgroup");
    let watch = WriteWatch::on(&cg, "cgroup.kill");
    let before = node.kills();
    node.delete(&id).unwrap();
    let after = node.kills();
    assert_eq!(
        (after.through_init, after.fell_back, after.cgroup_kill),
        (
            before.through_init + 1,
            before.fell_back,
            before.cgroup_kill
        ),
        "the delete ended it through its init's pidfd: {before:?} -> {after:?}"
    );
    assert_ne!(
        watch.written(),
        Some(true),
        "the delete wrote the box's cgroup.kill"
    );
    assert!(!cg.exists(), "the box's cgroup is gone");
    assert_eq!(
        program.wait_for(PATIENCE).unwrap(),
        Some(Exit::Signal(9)),
        "the exec'd program ended with its namespace"
    );
    assert!(
        !Path::new(&format!("/proc/{pid}")).exists(),
        "the exec'd program was reaped: no zombie of it"
    );
    eprintln!("test 1, own pid namespace: ended through the init's pidfd, no cgroup.kill write");

    // The second: a process outside the box's pid namespace in its cgroup.
    let id = BoxId::new("outsider").unwrap();
    node.create(lab.oci("outsider", None, Caps::Docker, Bounds::default()))
        .unwrap();
    node.start(&id).unwrap();
    let cg = node.cgroup_dir(&id).expect("the box's cgroup");
    let mut outside = Command::new("sleep").arg("600").spawn().expect("sleep");
    let outside_fd = pidfd(outside.id());
    std::fs::write(cg.join("cgroup.procs"), outside.id().to_string())
        .expect("move a process from outside into the box's cgroup");
    let watch = WriteWatch::on(&cg, "cgroup.kill");
    let before = node.kills();
    node.delete(&id).unwrap();
    let after = node.kills();
    assert_eq!(
        (after.through_init, after.fell_back, after.cgroup_kill),
        (
            before.through_init,
            before.fell_back + 1,
            before.cgroup_kill
        ),
        "the delete fell back to cgroup.kill: {before:?} -> {after:?}"
    );
    assert_ne!(
        watch.written(),
        Some(false),
        "the fallback wrote no cgroup.kill"
    );
    wait_for(&KILLED, || has_ended(&outside_fd), "the outsider killed");
    // Reaped here unless the node's sweep reaped it first.
    let _ = outside.wait();
    assert!(!cg.exists(), "the box's cgroup is gone");
    eprintln!("test 1, outsider: fell back to cgroup.kill, counted, the outsider killed");

    // The third: a process box.
    let id = BoxId::new("process").unwrap();
    node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Process,
        owner: None,
    })
    .unwrap();
    let mut program = node
        .exec(&id, Program::of(&["sleep", "600"]), Stdio::null())
        .unwrap();
    let cg = node.cgroup_dir(&id).expect("the box's cgroup");
    let watch = WriteWatch::on(&cg, "cgroup.kill");
    let before = node.kills();
    node.delete(&id).unwrap();
    let after = node.kills();
    assert_eq!(
        (after.through_init, after.fell_back, after.cgroup_kill),
        (
            before.through_init,
            before.fell_back,
            before.cgroup_kill + 1
        ),
        "a process box is ended by cgroup.kill: {before:?} -> {after:?}"
    );
    assert_ne!(
        watch.written(),
        Some(false),
        "the delete wrote no cgroup.kill for the process box"
    );
    assert_eq!(program.wait_for(PATIENCE).unwrap(), Some(Exit::Signal(9)));
    eprintln!("test 1, process box: ended by cgroup.kill, counted");

    // The fourth: an oci box whose init ended on its own and was reaped before the delete, its exit read.
    let id = BoxId::new("ended").unwrap();
    node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Bundle(
            lab.bundle_running(Caps::Docker, &["/hex", "selftest-box", "fail", "0"]),
        ),
        owner: None,
    })
    .unwrap();
    node.start(&id).unwrap();
    wait_for(
        &INIT_ENDS,
        || {
            node.inspect(&id)
                .is_ok_and(|info| info.exit == Some(Exit::Code(0)))
        },
        "the init's exit, read by inspect",
    );
    let cg = node.cgroup_dir(&id).expect("the box's cgroup");
    let watch = WriteWatch::on(&cg, "cgroup.kill");
    let before = node.kills();
    node.delete(&id).unwrap();
    let after = node.kills();
    assert_eq!(
        (after.through_init, after.fell_back, after.cgroup_kill),
        (
            before.through_init + 1,
            before.fell_back,
            before.cgroup_kill
        ),
        "a box whose init had ended needs no kill: {before:?} -> {after:?}"
    );
    assert_ne!(
        watch.written(),
        Some(true),
        "the delete wrote the box's cgroup.kill"
    );
    assert!(!cg.exists(), "the box's cgroup is gone");
    eprintln!("test 1, init ended before the delete: no kill, no cgroup.kill write");

    // The fifth, as the bench's throughput and sustained phases run them (the lead's finding 3(a),
    // 2026-11-05T07:50Z): one owner's boxes whose init ends at once, each deleted right after its start,
    // from many threads, so some deletes come as the supervisor collects the init's exit into the box's
    // record (supervise.rs, collect).
    let o = BoxOwner::named("owner-ends").unwrap();
    let bundle = lab.bundle_running(Caps::Docker, &["/hex", "selftest-box", "fail", "0"]);
    // How many: enough that the old delete, at the rate it was measured to kill, fails this case with 99%
    // odds. The A/B (unit hex-integrate-ab3a-20261106T092743Z, as root, 32 threads) saw the old delete kill
    // 2, 2 and 4 of 2,048 boxes, 8 in 6,144; a case of n boxes misses every kill with odds (1 - rate)^n,
    // under 1% once n >= ln(100) / rate. Thirty-two threads, the concurrency the rate was measured at.
    let threads: u64 = 32;
    let rate = 8.0_f64 / 6_144.0;
    let (mut boxes, mut missed) = (0_u64, 1.0_f64);
    while missed >= 0.01 {
        missed *= 1.0 - rate;
        boxes += 1;
    }
    let each = boxes.div_ceil(threads);
    let before = node.kills();
    std::thread::scope(|s| {
        for t in 0..threads {
            let (node, bundle, o) = (&node, &bundle, &o);
            s.spawn(move || {
                for n in 0..each {
                    let id = BoxId::new(&format!("ends-{t}-{n}")).unwrap();
                    node.create(BoxSpec {
                        route: None,
                        allocation: None,
                        devices: Vec::new(),
                        id: id.clone(),
                        bounds: Bounds::default(),
                        class: ClassSpec::Bundle(bundle.clone()),
                        owner: Some(o.clone()),
                    })
                    .unwrap();
                    node.start(&id).unwrap();
                    node.delete(&id).unwrap();
                }
            });
        }
    });
    let after = node.kills();
    assert_eq!(
        (after.through_init, after.fell_back, after.cgroup_kill),
        (
            before.through_init + threads * each,
            before.fell_back,
            before.cgroup_kill
        ),
        "every box whose init ended at once was ended without a kill: {before:?} -> {after:?}"
    );
    eprintln!(
        "test 1, {} inits ending at their deletes: no kill",
        threads * each
    );
}

/// A cgroup file's value, trimmed.
fn read(cg: &Path, file: &str) -> String {
    std::fs::read_to_string(cg.join(file))
        .unwrap_or_else(|e| panic!("{}/{file}: {e}", cg.display()))
        .trim()
        .to_owned()
}

/// The value of `key` in a flat-keyed cgroup file.
fn flat(cg: &Path, file: &str, key: &str) -> u64 {
    hex_core::cgroup::flat(&read(cg, file), key).unwrap_or(0)
}

/// What test 2's box A holds: 64 MiB, the bar B's own peak is held under. An idle cgroup handed on carries at most
/// 7,471,104 bytes of A's charge (reuse_residual.md, test 2's six runs and the root lab's), none of it A's own memory,
/// so a B whose peak reaches A's hold reports A's peak, not its own.
const A_HOG: u64 = 64 << 20;

/// The most CPU a whole lifecycle took in reuse_pass.md's records, the agent's and its box's together: 6.64 ms
/// (run 13, 20261103T185413Z, concurrency 32 under about 7,500 host threads). B's start is a part of one, so B
/// reports less than this as its own; A burns past it, so B reporting A's CPU fails. It was 1% of A's, picked
/// (Ada's 2026-11-06 rule).
const LIFECYCLE_CPU_MOST_US: u64 = 6_640;

/// Test 2. Do: box A of owner O burns CPU, holds 64 MiB, has a program OOM-killed and forks refused, and
/// is deleted; then box B of owner O is created and started. Expect: B is given A's slot; B's reported
/// CPU is under the most a lifecycle took on record, its peak under what A held, its OOM kills and refused forks 0,
/// while the cgroup's own files still count A's; B's memory bound and the default of the pids bound A set and B
/// doesn't are in force.
fn a_box_of_one_owner_is_given_the_cgroup_its_earlier_box_left(lab: &Lab, own: &Path) {
    let (node, _state) = lab.node(&own.join("hex-reuse-ab"));
    let o = BoxOwner::named("owner-o").unwrap();
    let a = BoxId::new("a").unwrap();
    let a_bounds = Bounds {
        memory_max: Some(128 << 20),
        pids_max: Some(32),
        ..Bounds::default()
    };
    node.create(lab.oci("a", Some(&o), Caps::Docker, a_bounds))
        .unwrap();
    node.start(&a).unwrap();
    let slot = node.cgroup_dir(&a).expect("A's cgroup");
    let name = slot.file_name().unwrap().to_str().unwrap().to_owned();
    assert!(
        hex_core::reuse::slot_of(&name).is_some(),
        "an owned box's cgroup is a slot: {name}"
    );
    let burn = Program::of(&[
        "/bin/sh",
        "-c",
        "head -c 3000000000 /dev/zero | sha256sum >/dev/null",
    ]);
    assert_eq!(run(&node, &a, burn, &BURN_ENDS), Exit::Code(0));
    assert_eq!(
        run(&node, &a, selftest(&Act::Hog(A_HOG)), &HOG_ENDS),
        Exit::Code(0)
    );
    assert_eq!(
        run(&node, &a, selftest(&Act::Hog(512 << 20)), &OOM_ENDS),
        Exit::Signal(9),
        "a program past A's memory bound is OOM-killed"
    );
    assert_ne!(
        run(&node, &a, selftest(&Act::Spawn(64)), &REFUSED_ENDS),
        Exit::Code(0),
        "a program past A's process bound fails"
    );
    let ua = node.usage(&a).unwrap();
    // Non-vacuity: A's CPU and peak are past what B may report as its own, so B's counts carried over from A's would
    // fail B's checks below.
    assert!(
        ua.cpu_usec > LIFECYCLE_CPU_MOST_US
            && ua.memory_peak_bytes >= A_HOG
            && ua.oom_kills >= 1
            && ua.pids_refused >= 1,
        "A used what the test made it use: {ua:?}"
    );
    node.delete(&a).unwrap();
    assert!(slot.exists(), "A's cgroup is kept idle for its owner");
    let stat = read(&slot, "memory.stat");
    let kinds: Vec<String> = [
        "anon",
        "file",
        "kernel",
        "shmem",
        "file_dirty",
        "slab",
        "percpu",
        "sock",
    ]
    .iter()
    .map(|k| format!("{k} {}", hex_core::cgroup::flat(&stat, k).unwrap_or(0)))
    .collect();
    eprintln!(
        "test 2: A left {} bytes charged in its idle cgroup ({})",
        read(&slot, "memory.current"),
        kinds.join(", ")
    );
    let before = node.cgroup_reuse().unwrap();
    let b = BoxId::new("b").unwrap();
    node.create(lab.oci(
        "b",
        Some(&o),
        Caps::Docker,
        Bounds {
            memory_max: Some(256 << 20),
            ..Bounds::default()
        },
    ))
    .unwrap();
    let after = node.cgroup_reuse().unwrap();
    assert_eq!(
        node.cgroup_dir(&b).as_deref(),
        Some(slot.as_path()),
        "B is given A's slot: {after:?}"
    );
    assert_eq!(after.pool.handed_on, before.pool.handed_on + 1);
    node.start(&b).unwrap();
    let ub = node.usage(&b).unwrap();
    assert!(
        ub.cpu_usec < LIFECYCLE_CPU_MOST_US,
        "B's CPU {} us is its own, under the most a whole lifecycle took on record ({LIFECYCLE_CPU_MOST_US} us), \
         not A's {} us",
        ub.cpu_usec,
        ua.cpu_usec
    );
    assert!(
        ub.memory_peak_bytes < A_HOG,
        "B's peak {} is its own, under the {A_HOG} bytes A's program held, not A's {}",
        ub.memory_peak_bytes,
        ua.memory_peak_bytes
    );
    assert_eq!(
        (ub.oom_kills, ub.pids_refused),
        (0, 0),
        "B's events: {ub:?}"
    );
    // The cgroup's own counters run on from A's: what B is told is a delta, not a fresh cgroup's.
    assert!(flat(&slot, "memory.events", "oom_kill") >= 1);
    assert!(flat(&slot, "pids.events", "max") >= 1);
    assert!(flat(&slot, "cpu.stat", "usage_usec") >= ua.cpu_usec);
    // Every bound written again: B's own, and A's pids bound back at its default.
    assert_eq!(read(&slot, "memory.max"), (256u64 << 20).to_string());
    assert_eq!(read(&slot, "pids.max"), "max");
    eprintln!(
        "test 2: B in A's slot, CPU {} us against A's {}, peak {} against {}, events 0",
        ub.cpu_usec, ua.cpu_usec, ub.memory_peak_bytes, ua.memory_peak_bytes
    );
    node.delete(&b).unwrap();
    node.release_idle().unwrap();
    assert!(!slot.exists(), "released, no idle cgroup is left");
}

/// The processes in a cgroup, by pid, sorted.
fn procs(cg: &Path) -> Vec<u32> {
    let mut pids: Vec<u32> = read(cg, "cgroup.procs")
        .lines()
        .map(|l| l.trim().parse().unwrap())
        .collect();
    pids.sort_unstable();
    pids
}

/// A process's state and session, as /proc/<pid>/stat has them (proc(5), fields 3 and 6), read after its name's
/// closing parenthesis, the line's last, since the name may hold spaces and parentheses itself.
fn state_and_session(pid: u32) -> (String, u32) {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let (_, fields) = stat.rsplit_once(')').unwrap();
    let mut fields = fields.split_whitespace();
    let state = fields.next().unwrap().to_owned();
    // After the state: the parent's pid, the process group's, then the session's.
    (state, fields.nth(2).unwrap().parse().unwrap())
}

/// The processes in cgroup `cg` once it holds any, waiting on the fact through `cgroup.events`, which the kernel
/// marks changed (`POLLPRI`) when the cgroup is populated (cgroup-v2.rst, "cgroup.events"); whatever it holds
/// at `patience`'s end, perhaps none.
fn populated_within(cg: &Path, patience: Duration) -> Vec<u32> {
    let deadline = Instant::now().checked_add(patience);
    loop {
        // Opened before the read, so a change after the read is seen by the poll.
        let events = std::fs::File::open(cg.join("cgroup.events")).unwrap();
        let now = procs(cg);
        let left = deadline.map_or(patience, |d| d.saturating_duration_since(Instant::now()));
        if !now.is_empty() || left.is_zero() {
            return now;
        }
        let mut fds = [PollFd::new(&events, PollFlags::PRI)];
        let timeout = Timespec {
            tv_sec: i64::try_from(left.as_secs()).unwrap(),
            tv_nsec: left.subsec_nanos().into(),
        };
        let _ = rustix::event::poll(&mut fds, Some(&timeout));
    }
}

/// Goal 5's p99.9 tail (decision 8, "The p99.9 that stayed"; AC-T7.1.3): an owner's idle cgroup holds an init made
/// ahead, so the owner's next create clones nothing into a cgroup and takes no `cgroup_mutex`. Do: A of O created,
/// started and deleted; wait for the fact that A's idle slot holds one process; create B of O. Expect: the slot holds
/// that one process, with no effective capability and, once it sleeps for its claim, leading a session of its own,
/// before B's create and the same one after it, so B's init is the parked one; B starts and runs a program; the node
/// counts one init claimed. Then delete B and release the idle cgroups: none is left, so a parked init is ended with
/// its cgroup. With parked procs (root only), B's create takes the proc made for that init at A's delete, counted a hit
/// (decision 8, parked procs).
fn an_owners_next_box_is_made_by_the_init_parked_in_its_idle_cgroup(lab: &Lab, own: &Path) {
    let (node, _state) = lab.node(&own.join("hex-reuse-parked"));
    let o = BoxOwner::named("owner-p").unwrap();
    let a = BoxId::new("pa").unwrap();
    node.create(lab.oci("pa", Some(&o), Caps::Docker, Bounds::default()))
        .unwrap();
    node.start(&a).unwrap();
    let slot = node.cgroup_dir(&a).expect("A's cgroup");
    node.delete(&a).unwrap();
    // With parked procs (root only), A's delete made the init's proc itself, before it returned: the park's own
    // thread, never a create's (the lead's 15:40Z).
    if !lab.rootless {
        let made = node.cgroup_reuse().unwrap().parked;
        assert_eq!(
            (
                made.procs_made,
                made.procs_made_at_delete,
                made.procs_made_at_refill,
                made.procs_held
            ),
            (1, 1, 0, 1),
            "A's delete made the parked init's proc before it returned: {made:?}"
        );
    }
    // Its delete parks the slot's init before it answers (Cgroups::give_back): the slot is held at the first look.
    assert!(
        !procs(&slot).is_empty(),
        "the delete parked the slot's init before it answered"
    );
    let parked = populated_within(&slot, PARKED.deadline().unwrap_or(Duration::MAX));
    assert_eq!(
        parked.len(),
        1,
        "A's idle slot holds one init made ahead for O's next box: {parked:?}, {:?}",
        node.cgroup_reuse().unwrap().parked
    );
    // Parked, it exercises no capability: its effective set is empty (proc(5), /proc/<pid>/status `CapEff`).
    let status = std::fs::read_to_string(format!("/proc/{}/status", parked[0])).unwrap();
    let eff = status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .map(str::trim)
        .unwrap();
    assert_eq!(
        u64::from_str_radix(eff, 16).unwrap(),
        0,
        "a parked init holds no effective capability"
    );
    // And it leads a session of its own before a create claims it (goal 17, runtime design goal 6, lever L1): its
    // `setsid`, and the reschedule its move to a new autogroup forces (kernel/sched/core.c `sched_move_task`,
    // `resched_curr`), are its park's, never the claiming create's. It leads it once it has said it is parked, so the
    // delete doesn't wait on that either: looked at once it sleeps for its claim, its only sleep after that word
    // (proc(5), /proc/<pid>/stat: the state `S` and the session, fields 3 and 6).
    let mut asleep = (String::new(), 0);
    wait_for(
        &PARKED_ASLEEP,
        || {
            asleep = state_and_session(parked[0]);
            asleep.0 == "S"
        },
        "the parked init asleep for its claim",
    );
    assert_eq!(
        asleep.1, parked[0],
        "a parked init leads a session of its own before its claim (its state and session: {asleep:?})"
    );
    // Nor can another process take its claim socket or any descriptor of its: it is non-dumpable, so a caller
    // without CAP_SYS_PTRACE is refused pidfd_getfd (ptrace(2), "Ptrace access mode checking"). This process is
    // one when rootless; as root it holds CAP_SYS_PTRACE, which reaches any process, the node's own included.
    if lab.rootless {
        let init = pidfd(parked[0]);
        for fd in 0..64 {
            assert_eq!(
                rustix::process::pidfd_getfd(&init, fd, rustix::process::PidfdGetfdFlags::empty())
                    .map(drop)
                    .map_err(|e| e == rustix::io::Errno::PERM),
                Err(true),
                "another process took descriptor {fd} of a parked init"
            );
        }
    }
    let before = node.cgroup_reuse().unwrap();
    let b = BoxId::new("pb").unwrap();
    node.create(lab.oci("pb", Some(&o), Caps::Docker, Bounds::default()))
        .unwrap();
    let after = node.cgroup_reuse().unwrap();
    assert_eq!(node.cgroup_dir(&b).as_deref(), Some(slot.as_path()));
    assert_eq!(
        procs(&slot),
        parked,
        "B's init is the one parked in the slot, not a clone of B's create"
    );
    assert_eq!(after.parked.claimed, before.parked.claimed + 1, "{after:?}");
    if !lab.rootless {
        assert_eq!(
            (after.parked.proc_hits, after.parked.proc_misses),
            (before.parked.proc_hits + 1, before.parked.proc_misses),
            "B took the proc made at A's delete: {:?}",
            after.parked
        );
    }
    node.start(&b).unwrap();
    assert_eq!(
        run(&node, &b, Program::of(&["/bin/true"]), &TRUE_ENDS),
        Exit::Code(0)
    );
    node.delete(&b).unwrap();
    node.release_idle().unwrap();
    assert!(
        !slot.exists(),
        "released, no idle cgroup nor parked init is left"
    );
    eprintln!("test 2b: B made by the init parked in A's slot (pid {parked:?})");
}

/// The lead's (A) of 2026-11-09 (decision 8's claim, goal 16): a create that claims a parked init wakes its agent once
/// for each word the init says on its sync pipe, and not for an answer to the claim, which the init sent as it took
/// it. The init's words: its ask for its user namespace's maps where its box makes one (a rootless node's does), then
/// its ready. Do: O's first box created, started and deleted, so its slot holds an init parked for O's next; then three
/// times, once that init sleeps for its claim, create O's next box, counting the creating thread's voluntary context
/// switches across the create (proc(5), /proc/thread-self/status), then start and delete it, which parks the next.
/// Expect: each create claimed the parked init, and the median of the three counts is at most the init's words,
/// one as root and two rootless: a word buffered before its read needs no sleep. The median, since a create's other steps sleep now and then (a lock one of the node's threads
/// holds, the page cache), and a create can find a word written by the time it reads it: of 59 claims each way on
/// adacachy2 before this (unit hex-g6-wakes-20261109T181009Z), 54 as root slept twice, for the answer and the ready, 1
/// once more and 4 once less; 57 rootless slept twice too, the ask for its maps written by the time the answer was
/// read, and 2 once more. Three is the fewest creates whose median one such create can't move.
fn a_parked_create_wakes_its_agent_once_a_word(lab: &Lab, own: &Path) {
    let (node, _state) = lab.node(&own.join("hex-reuse-wakes"));
    let o = BoxOwner::named("owner-w").unwrap();
    // The creating thread's voluntary context switches so far: each a sleep it woke from (proc(5)).
    let slept = || -> u64 {
        std::fs::read_to_string("/proc/thread-self/status")
            .unwrap()
            .lines()
            .find_map(|l| l.strip_prefix("voluntary_ctxt_switches:"))
            .unwrap()
            .trim()
            .parse()
            .unwrap()
    };
    // Starts and deletes box `name`, whose delete parks an init in its slot for O's next box, and waits for that init
    // to sleep for its claim.
    let park = |name: &str| {
        let id = BoxId::new(name).unwrap();
        node.start(&id).unwrap();
        let slot = node.cgroup_dir(&id).expect("the box's cgroup");
        node.delete(&id).unwrap();
        let parked = populated_within(&slot, PARKED.deadline().unwrap_or(Duration::MAX));
        assert_eq!(
            parked.len(),
            1,
            "{name}'s delete parked one init: {parked:?}"
        );
        wait_for(
            &PARKED_ASLEEP,
            || state_and_session(parked[0]) == ("S".to_owned(), parked[0]),
            "the parked init asleep for its claim",
        );
    };
    node.create(lab.oci("wake-0", Some(&o), Caps::Docker, Bounds::default()))
        .unwrap();
    park("wake-0");
    let mut wakes = Vec::new();
    for name in ["wake-1", "wake-2", "wake-3"] {
        let claimed = node.cgroup_reuse().unwrap().parked.claimed;
        let before = slept();
        node.create(lab.oci(name, Some(&o), Caps::Docker, Bounds::default()))
            .unwrap();
        wakes.push(slept() - before);
        assert_eq!(
            node.cgroup_reuse().unwrap().parked.claimed,
            claimed + 1,
            "{name}'s create claimed the parked init"
        );
        park(name);
    }
    // The init's words on its sync pipe: its ask for its user namespace's maps, where its box makes one (the lab's
    // rootless bundles do, `Lab::bundle_with`; its root ones don't), and its ready.
    let words = if lab.rootless { 2 } else { 1 };
    wakes.sort_unstable();
    assert!(
        wakes[1] <= words,
        "a parked create wakes its agent at most once for each word its init says, and for no answer (each create's wakes, \
         sorted: {wakes:?})"
    );
    node.release_idle().unwrap();
    eprintln!("wakes: three parked creates woke their agent {wakes:?} times");
}

/// Decision 8, "The p99.9 that stayed" (AC-T7.1.3): an init parked for an owner's next box with namespaces its spec
/// doesn't ask for is never used. Do: A of O, which has an ipc namespace of its own, created and deleted, so its
/// slot holds an init parked for one; B of O, whose spec makes no ipc namespace, created. Expect: B in A's slot,
/// the parked init ended, not claimed, and counted ended unclaimed; B starts and runs a program.
fn a_parked_init_made_with_other_namespaces_is_ended_and_counted(lab: &Lab, own: &Path) {
    let (node, _state) = lab.node(&own.join("hex-reuse-parkflags"));
    let o = BoxOwner::named("owner-f").unwrap();
    let a = BoxId::new("fa").unwrap();
    node.create(lab.oci("fa", Some(&o), Caps::Docker, Bounds::default()))
        .unwrap();
    let slot = node.cgroup_dir(&a).expect("A's cgroup");
    node.delete(&a).unwrap();
    // Its delete parks the slot's init before it answers (Cgroups::give_back): the slot is held at the first look.
    assert!(
        !procs(&slot).is_empty(),
        "the delete parked the slot's init before it answered"
    );
    let parked = populated_within(&slot, PARKED.deadline().unwrap_or(Duration::MAX));
    assert_eq!(
        parked.len(),
        1,
        "A's idle slot holds a parked init: {parked:?}"
    );
    let init = pidfd(parked[0]);
    // B's bundle: A's, its ipc namespace taken out (a pid namespace can't be: rootless, proc mounts only for a pid
    // namespace of the container's own user namespace).
    let bundle = lab.bundle(Caps::Docker);
    let config = std::fs::read_to_string(bundle.join("config.json")).unwrap();
    assert!(config.contains(r#"{"type":"ipc"},"#));
    std::fs::write(
        bundle.join("config.json"),
        config.replace(r#"{"type":"ipc"},"#, ""),
    )
    .unwrap();
    let before = node.cgroup_reuse().unwrap().parked;
    let b = BoxId::new("fb").unwrap();
    node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: b.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Bundle(bundle),
        owner: Some(o.clone()),
    })
    .unwrap();
    let after = node.cgroup_reuse().unwrap().parked;
    assert_eq!(
        node.cgroup_dir(&b).as_deref(),
        Some(slot.as_path()),
        "B in A's slot"
    );
    assert_eq!(
        (after.claimed, after.ended_unclaimed),
        (before.claimed, before.ended_unclaimed + 1),
        "the parked init, made for an ipc namespace B's spec doesn't ask for, ended unclaimed: {after:?}"
    );
    assert!(has_ended(&init), "the parked init has ended");
    node.start(&b).unwrap();
    assert_eq!(
        run(&node, &b, Program::of(&["/bin/true"]), &TRUE_ENDS),
        Exit::Code(0)
    );
    node.delete(&b).unwrap();
    node.release_idle().unwrap();
    eprintln!("parked init made with other namespaces: ended, counted");
}

/// Parked procs are a root node's, with no switch to turn them off (decision 8, parked procs; the lead, 2026-11-06;
/// Ada's 2026-11-06 rule removed the off path once bench/competitive/probes/parked_procs_ab.md decided them on;
/// AC-T7.1.3). Do: in a node opened with no setting, A of O created, started and deleted. Expect: as root, the delete
/// made the parked init's proc (one made, at the delete), and the node says it parks procs; rootless, none made and
/// it says it parks none (a rootless node has no proc maker).
fn parked_procs_are_a_root_nodes_and_a_rootless_node_makes_none(lab: &Lab, own: &Path) {
    let (node, _state) = lab.node(&own.join("hex-reuse-ppdefault"));
    let o = BoxOwner::named("owner-ppd").unwrap();
    let a = BoxId::new("ppd").unwrap();
    node.create(lab.oci("ppd", Some(&o), Caps::Docker, Bounds::default()))
        .unwrap();
    node.start(&a).unwrap();
    node.delete(&a).unwrap();
    let parked = node.cgroup_reuse().unwrap().parked;
    node.release_idle().unwrap();
    let want = if lab.rootless { (0, 0) } else { (1, 1) };
    assert_eq!(
        (
            (parked.procs_made, parked.procs_made_at_delete),
            node.parked_procs()
        ),
        (want, !lab.rootless),
        "procs made (in all, at the delete), and whether the node parks them (rootless {})",
        lab.rootless
    );
}

/// Decision 8, parked procs (AC-T7.1.3; the lead's 15:57Z): at most a bound of procs alive at once, held with parked
/// inits or waiting for the dropper, whose queue holds as many, so a tree handed to it never finds it full. Root
/// only. Do: on a node bounded at 2, four owners each create and delete a box, so four inits park. Expect 2 procs made
/// and 2 parks capped, 2 alive. Then release the idle cgroups: expect the 2 trees queued to the dropper and dropped
/// there, none inline, none alive. Then a fifth owner's delete: its proc made, the bound free again.
fn parked_procs_stop_at_their_bound_and_none_drops_inline(lab: &Lab, own: &Path) {
    if lab.rootless {
        return;
    }
    let (node, _state) = lab.node(&own.join("hex-reuse-procbound"));
    let node = node.with_parked_procs_bound(2).unwrap();
    let cycle = |owner: &str, id: &str| {
        let o = BoxOwner::named(owner).unwrap();
        node.create(lab.oci(id, Some(&o), Caps::Docker, Bounds::default()))
            .unwrap();
        node.delete(&BoxId::new(id).unwrap()).unwrap();
    };
    for n in 1..=4 {
        cycle(&format!("owner-b{n}"), &format!("pb{n}"));
    }
    let full = node.cgroup_reuse().unwrap().parked;
    assert_eq!(
        (
            full.parked,
            full.procs_made,
            full.procs_capped,
            full.procs_alive,
            full.procs_held
        ),
        (4, 2, 2, 2, 2),
        "two procs at the bound, two parks capped: {full:?}"
    );
    node.release_idle().unwrap();
    wait_for(
        &DROPPED,
        || node.cgroup_reuse().unwrap().parked.procs_dropped == 2,
        "the two trees dropped by the dropper",
    );
    let drained = node.cgroup_reuse().unwrap().parked;
    assert_eq!(
        (
            drained.procs_queued,
            drained.procs_dropped_inline,
            drained.procs_alive,
            drained.procs_held
        ),
        (2, 0, 0, 0),
        "at the bound, every tree went to the dropper, none inline: {drained:?}"
    );
    cycle("owner-b5", "pb5");
    let again = node.cgroup_reuse().unwrap().parked;
    assert_eq!(
        (again.procs_made, again.procs_capped, again.procs_alive),
        (3, 2, 1),
        "the bound free again: {again:?}"
    );
    node.release_idle().unwrap();
    eprintln!("parked procs at their bound: 2 made, 2 capped, none dropped inline");
}

/// Each slot cgroup under `root` that holds a process, with its processes.
fn populated_slots(root: &Path) -> Vec<(PathBuf, Vec<u32>)> {
    let mut out: Vec<(PathBuf, Vec<u32>)> = std::fs::read_dir(root)
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| hex_core::reuse::slot_of(n).is_some())
        })
        .map(|e| (e.path(), procs(&e.path())))
        .filter(|(_, p)| !p.is_empty())
        .collect();
    out.sort();
    out
}

/// Decision 8, "The p99.9 that stayed" (AC-T7.1.3), reuse off: a node keeps cgroups made ahead for boxes of no
/// owner, each with an init parked in it. Do: on a node keeping one, create and delete box N1 of no owner (which
/// finds none ready, and says the namespaces its init was made with); wait for the fact that one is ready; create
/// N2 of no owner. Expect: N2's cgroup is the slot made ahead and its init the one parked there; it starts and runs
/// a program; one taken and one claimed counted, N1's create counted dry. Then delete N2 and release the idle
/// cgroups: N2's slot is removed with it, never kept, and nothing made ahead is left.
fn a_box_of_no_owner_is_made_by_an_init_parked_in_a_cgroup_made_ahead(lab: &Lab, own: &Path) {
    let (node, _state) = lab.node(&own.join("hex-reuse-ahead"));
    let node = node.with_inits_ahead(1);
    let n1 = BoxId::new("n1").unwrap();
    node.create(lab.oci("n1", None, Caps::Docker, Bounds::default()))
        .unwrap();
    let root = node
        .cgroup_dir(&n1)
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    node.delete(&n1).unwrap();
    wait_for(
        &MADE_AHEAD,
        || node.cgroup_reuse().unwrap().ahead.ready != 0,
        "a cgroup made ahead",
    );
    let before = node.cgroup_reuse().unwrap();
    assert_eq!(
        before.ahead.ready, 1,
        "one cgroup made ahead: {:?}",
        before.ahead
    );
    assert!(
        before.ahead.dry >= 1,
        "N1 found none ready: {:?}",
        before.ahead
    );
    let ready = populated_slots(&root);
    assert_eq!(
        ready.len(),
        1,
        "the one made ahead holds its parked init: {ready:?}"
    );
    let n2 = BoxId::new("n2").unwrap();
    node.create(lab.oci("n2", None, Caps::Docker, Bounds::default()))
        .unwrap();
    let after = node.cgroup_reuse().unwrap();
    let slot = node.cgroup_dir(&n2).unwrap();
    assert_eq!(
        (slot.clone(), procs(&slot)),
        ready[0].clone(),
        "N2 is made in the cgroup made ahead, by the init parked there"
    );
    assert_eq!(
        after.ahead.taken,
        before.ahead.taken + 1,
        "{:?}",
        after.ahead
    );
    assert_eq!(
        after.parked.claimed,
        before.parked.claimed + 1,
        "{:?}",
        after.parked
    );
    node.start(&n2).unwrap();
    assert_eq!(
        run(&node, &n2, Program::of(&["/bin/true"]), &TRUE_ENDS),
        Exit::Code(0)
    );
    node.delete(&n2).unwrap();
    assert!(
        !slot.exists(),
        "a box of no owner's slot is removed with it, never kept"
    );
    node.release_idle().unwrap();
    assert_eq!(populated_slots(&root), vec![], "nothing made ahead is left");
    eprintln!("test 2c: N2 made by the init parked in {}", slot.display());
}

/// Goal 16 (AC-T7.1.3; bench/competitive/probes/goal16_ab.md, "Whose work the no-owner tail waits on"): the slowest
/// no-owner creates waited on their own cgroup file lookups, queued behind the refill's `mkdir` of the next cgroup made
/// ahead. Do: on a node keeping one cgroup ahead, N1 of no owner created and deleted (so the refill starts), the fact
/// waited for that one is ready, then N2 of no owner created on this thread. Expect: N2 in the cgroup made ahead, and
/// no cgroupfs lookup on this thread across N2's create (its files opened by the refill, its id's absence known
/// without one). Then N2 runs a program and is deleted, nothing made ahead left once released.
fn a_box_of_no_owner_made_ahead_looks_up_nothing_in_cgroupfs(lab: &Lab, own: &Path) {
    let (node, _state) = lab.node(&own.join("hex-reuse-nolookup"));
    let node = node.with_inits_ahead(1);
    let n1 = BoxId::new("q1").unwrap();
    node.create(lab.oci("q1", None, Caps::Docker, Bounds::default()))
        .unwrap();
    node.delete(&n1).unwrap();
    wait_for(
        &MADE_AHEAD,
        || node.cgroup_reuse().unwrap().ahead.ready >= 1,
        "a cgroup made ahead",
    );
    let taken = node.cgroup_reuse().unwrap().ahead.taken;
    let spec = lab.oci(
        "q2",
        None,
        Caps::Docker,
        Bounds {
            memory_max: Some(256 << 20),
            pids_max: Some(256),
            ..Bounds::default()
        },
    );
    let before = hex_runtime::linux::cgroup::lookups_on_this_thread();
    node.create(spec).unwrap();
    let lookups = hex_runtime::linux::cgroup::lookups_on_this_thread() - before;
    assert_eq!(
        node.cgroup_reuse().unwrap().ahead.taken,
        taken + 1,
        "N2 took the cgroup made ahead"
    );
    assert_eq!(lookups, 0, "N2's create looked up {lookups} cgroupfs names");
    let n2 = BoxId::new("q2").unwrap();
    node.start(&n2).unwrap();
    assert_eq!(
        run(&node, &n2, Program::of(&["/bin/true"]), &TRUE_ENDS),
        Exit::Code(0)
    );
    node.delete(&n2).unwrap();
    node.release_idle().unwrap();
    eprintln!("test 2f: a create in a cgroup made ahead, no cgroupfs lookup on its thread");
}

/// Goal 16 (AC-T7.1.3): a cgroup made ahead is fresh, made with no bound (`refill_ahead`), so it holds the kernel's
/// defaults, as a cgroup a create makes on its path does; a box of no owner given one has its own bounds written there
/// alone, as that create's are, never every default as a cgroup an earlier box left must have (`BoxRoot::rebound`).
/// For a memory and a pids bound that is three files where `rebound` writes about ten, `cpu.weight`'s write walking
/// every CPU's run queue and each disk's `io.latency` and `io.max` among them. Do: on a node keeping one cgroup ahead,
/// N1 of no owner created and deleted so the refill makes one, its slot watched for writes, then N2 of no owner created
/// with a memory and a pids bound. Expect: N2 in the watched slot, the files written there its bounds' (`memory.max`,
/// `memory.swap.max` where the kernel has it, `pids.max`) and its peak's reset (`memory.peak`), and no other.
fn a_box_of_no_owner_made_ahead_is_given_its_own_bounds_alone(lab: &Lab, own: &Path) {
    let (node, _state) = lab.node(&own.join("hex-reuse-ownbounds"));
    let node = node.with_inits_ahead(1);
    let n1 = BoxId::new("w1").unwrap();
    node.create(lab.oci("w1", None, Caps::Docker, Bounds::default()))
        .unwrap();
    let root = node
        .cgroup_dir(&n1)
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    node.delete(&n1).unwrap();
    wait_for(
        &MADE_AHEAD,
        || node.cgroup_reuse().unwrap().ahead.ready != 0,
        "a cgroup made ahead",
    );
    let ready = populated_slots(&root);
    assert_eq!(
        ready.len(),
        1,
        "the one made ahead holds its parked init: {ready:?}"
    );
    let slot = ready[0].0.clone();
    let watch = WriteWatch::on(&slot, "memory.max");
    let bounds = Bounds {
        memory_max: Some(256 << 20),
        pids_max: Some(256),
        ..Bounds::default()
    };
    node.create(lab.oci("w2", None, Caps::Docker, bounds))
        .unwrap();
    let n2 = BoxId::new("w2").unwrap();
    assert_eq!(
        node.cgroup_dir(&n2).as_deref(),
        Some(slot.as_path()),
        "N2 took the cgroup made ahead"
    );
    if let Some(written) = watch.files_written() {
        // Its peak was reset by the refill once its init parked (test 2h), so the take writes the box's bounds alone.
        let own: BTreeSet<String> = ["memory.max", "memory.swap.max", "pids.max"]
            .into_iter()
            .filter(|f| slot.join(f).exists())
            .map(str::to_owned)
            .collect();
        assert_eq!(
            written, own,
            "the files N2's create wrote in the slot made ahead"
        );
    }
    node.delete(&n2).unwrap();
    node.release_idle().unwrap();
    eprintln!("test 2g: a box of no owner made ahead given its own bounds alone");
}

/// Test 2h (goal 16; the breakdown's part 4, create-breakdown-20261109T164604Z: the claim's 0.030 ms p50, its reads of
/// the slot's counters among it). Do: wait for a cgroup made ahead, watch its files for reads and writes, and create a
/// box of no owner, which takes it. Expect: the take reads none of the counters a box's usage is reported from and
/// resets no peak, the refill having done both once the slot's init parked, as the init does nothing until taken; and
/// the box's usage still starts there: its CPU under a lifecycle's most, its OOM kills and refused forks 0.
fn a_box_of_no_owner_made_ahead_reads_none_of_its_counters(lab: &Lab, own: &Path) {
    let (node, _state) = lab.node(&own.join("hex-reuse-counters"));
    let node = node.with_inits_ahead(1);
    let n1 = BoxId::new("c1").unwrap();
    node.create(lab.oci("c1", None, Caps::Docker, Bounds::default()))
        .unwrap();
    let root = node
        .cgroup_dir(&n1)
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    node.delete(&n1).unwrap();
    wait_for(
        &MADE_AHEAD,
        || node.cgroup_reuse().unwrap().ahead.ready != 0,
        "a cgroup made ahead",
    );
    let ready = populated_slots(&root);
    assert_eq!(ready.len(), 1, "the one made ahead: {ready:?}");
    let slot = ready[0].0.clone();
    let Some(watch) = Touched::on(&slot) else {
        eprintln!("test 2h: SKIP, no inotify instance left for this user here (EMFILE)");
        node.release_idle().unwrap();
        return;
    };
    node.create(lab.oci("c2", None, Caps::Docker, Bounds::default()))
        .unwrap();
    let n2 = BoxId::new("c2").unwrap();
    assert_eq!(
        node.cgroup_dir(&n2).as_deref(),
        Some(slot.as_path()),
        "C2 took the cgroup made ahead"
    );
    let (read, written) = watch.files();
    let counters: BTreeSet<String> = ["memory.events", "pids.events", "cpu.stat", "io.stat"]
        .into_iter()
        .filter(|f| slot.join(f).exists())
        .map(str::to_owned)
        .collect();
    assert!(
        read.is_disjoint(&counters),
        "C2's take read the counters {:?}",
        read.intersection(&counters).collect::<Vec<_>>()
    );
    assert!(
        !written.contains("memory.peak"),
        "C2's take reset its peak: {written:?}"
    );
    let usage = node.usage(&n2).unwrap();
    assert!(
        usage.cpu_usec < LIFECYCLE_CPU_MOST_US && usage.oom_kills == 0 && usage.pids_refused == 0,
        "C2's usage starts where the refill read it: {usage:?}"
    );
    node.delete(&n2).unwrap();
    node.release_idle().unwrap();
    eprintln!(
        "test 2h: a box of no owner made ahead reads none of its counters on its create's path"
    );
}

/// The files of a cgroup read and written while it is watched (inotify's IN_ACCESS and IN_MODIFY on its directory:
/// the VFS notifies each read and write of a cgroupfs file as of any other), or `None` where no inotify instance is left.
struct Touched(OwnedFd);

impl Touched {
    fn on(cgroup: &Path) -> Option<Self> {
        let fd = match inotify::init(inotify::CreateFlags::CLOEXEC | inotify::CreateFlags::NONBLOCK)
        {
            Ok(fd) => fd,
            Err(rustix::io::Errno::MFILE) => return None,
            Err(e) => panic!("inotify: {e}"),
        };
        inotify::add_watch(
            &fd,
            cgroup,
            inotify::WatchFlags::ACCESS | inotify::WatchFlags::MODIFY,
        )
        .expect("watch the cgroup's directory");
        Some(Self(fd))
    }

    /// The files read and the files written since the watch began.
    fn files(&self) -> (BTreeSet<String>, BTreeSet<String>) {
        let (mut read, mut written) = (BTreeSet::new(), BTreeSet::new());
        let mut buf = [MaybeUninit::<u8>::uninit(); 4096];
        let mut events = inotify::Reader::new(&self.0, &mut buf);
        loop {
            match events.next() {
                Ok(e) => {
                    let Some(name) = e.file_name().map(|n| n.to_string_lossy().into_owned()) else {
                        continue;
                    };
                    if e.events().contains(inotify::ReadFlags::ACCESS) {
                        read.insert(name.clone());
                    }
                    if e.events().contains(inotify::ReadFlags::MODIFY) {
                        written.insert(name);
                    }
                }
                Err(rustix::io::Errno::AGAIN) => return (read, written),
                Err(e) => panic!("reading the cgroup's watch: {e}"),
            }
        }
    }
}

/// Decision 8, "The p99.9 that stayed" (AC-T7.1.3): recovery removes the inits a node parked. Do: A of O created and
/// deleted, so A's idle slot holds a parked init; the node abandoned as a crash leaves it (none of its teardown
/// runs: `ManuallyDrop`); a node opened again over the same box root and state, and its recovery run. Expect: the
/// idle slot is gone and the parked init has ended. With parked procs (root only), the init's proc was made at A's
/// delete; its tree is the crashed node's descriptor, gone with that process, and its pid namespace with the init.
fn recovery_removes_an_init_parked_by_a_node_that_crashed(lab: &Lab, own: &Path) {
    let box_root = own.join("hex-reuse-recover");
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let open = || {
        Node::open(&box_root, state.path(), state.path())
            .expect("open a node")
            .with_oci_helper(PathBuf::from(&lab.exe))
    };
    let node = open();
    let o = BoxOwner::named("owner-r").unwrap();
    let a = BoxId::new("ra").unwrap();
    node.create(lab.oci("ra", Some(&o), Caps::Docker, Bounds::default()))
        .unwrap();
    let slot = node.cgroup_dir(&a).unwrap();
    node.delete(&a).unwrap();
    // Its delete parks the slot's init before it answers (Cgroups::give_back): the slot is held at the first look.
    assert!(
        !procs(&slot).is_empty(),
        "the delete parked the slot's init before it answered"
    );
    let parked = populated_within(&slot, PARKED.deadline().unwrap_or(Duration::MAX));
    assert_eq!(
        parked.len(),
        1,
        "A's idle slot holds a parked init: {parked:?}"
    );
    if !lab.rootless {
        let held = node.cgroup_reuse().unwrap().parked;
        assert_eq!(held.procs_held, 1, "its proc made with it: {held:?}");
    }
    let init = pidfd(parked[0]);
    let _abandoned = std::mem::ManuallyDrop::new(node);
    let node = open();
    node.recover().unwrap();
    assert!(!slot.exists(), "recovery removed the idle slot");
    let mut fds = [PollFd::new(&init, PollFlags::IN)];
    // Recovery's kill of the parked init, then its end: a killed process's, `node.emptying`'s record. Whole seconds and
    // the nanoseconds past them: the seconds alone make a bound under a second a zero poll.
    let killed = EMPTYING.saturating_mul(2);
    let patience = Timespec {
        tv_sec: i64::try_from(killed.as_secs()).unwrap_or(i64::MAX),
        tv_nsec: killed.subsec_nanos().into(),
    };
    let _ = rustix::event::poll(&mut fds, Some(&patience));
    assert!(has_ended(&init), "recovery ended the parked init");
    node.release_idle().unwrap();
    eprintln!("test 2d: recovery removed the slot and ended the init parked in it");
}

/// With test 2, the lead's ruling on `cpu.max` (2026-11-03T01:19Z): a reused cgroup's `cpu.max` is
/// written again when the next box's values differ from what it holds, and left when they're the same,
/// compared as values (`hex_core::cgroup::CpuMax`). Do: A of owner O, bounded to half a CPU, is deleted;
/// B of O, bounded to a quarter, is created; B is deleted, and C of O, bounded to a quarter too, is
/// created. Expect: B and C each given A's slot, the file holding a quarter's values after each, and a
/// write to it seen for B and none for C.
fn a_reused_cgroups_cpu_max_is_written_only_when_its_values_differ(lab: &Lab, own: &Path) {
    let (node, _state) = lab.node(&own.join("hex-reuse-cpu"));
    let o = BoxOwner::named("owner-cpu").unwrap();
    let bounded = |milli| Bounds {
        cpu_max_milli: Some(milli),
        ..Bounds::default()
    };
    let a = BoxId::new("cpu-a").unwrap();
    node.create(lab.oci("cpu-a", Some(&o), Caps::Docker, bounded(500)))
        .unwrap();
    let slot = node.cgroup_dir(&a).unwrap();
    let held = || std::fs::read_to_string(slot.join("cpu.max")).unwrap();
    assert_eq!(held().trim(), "50000 100000", "A's half a CPU");
    node.delete(&a).unwrap();
    for (id, written) in [("cpu-b", true), ("cpu-c", false)] {
        let watch = WriteWatch::on(&slot, "cpu.max");
        let next = BoxId::new(id).unwrap();
        node.create(lab.oci(id, Some(&o), Caps::Docker, bounded(250)))
            .unwrap();
        assert_eq!(
            node.cgroup_dir(&next).as_deref(),
            Some(slot.as_path()),
            "{id} is given A's slot"
        );
        assert_eq!(held().trim(), "25000 100000", "{id}'s quarter of a CPU");
        if let Some(seen) = watch.written() {
            assert_eq!(
                seen, written,
                "{id}: cpu.max is written only when its values differ"
            );
        }
        node.delete(&next).unwrap();
    }
    eprintln!("cpu.max: written for B (a half to a quarter), left for C (a quarter to a quarter)");
    node.release_idle().unwrap();
}

/// Goal 5 extended, step 6 (Ada's call of 2026-11-03): a box's network namespace handed to its owner's next
/// box when the box couldn't have changed it and the check finds nothing of it left. Root only: only a root
/// node keeps namespaces (a rootless node's containers make theirs in user namespaces of their own). What a
/// box could leave in its namespace is done here from outside, by this process (root) on a thread that joins
/// it, while the box is alive: it is the namespace's state at the delete that the check reads, whoever made
/// it. Do, expect:
/// - A of O, eligible (Docker's capabilities, `/proc/sys` read-only), deleted; B of O: B is given A's
///   namespace (the same inode), counted handed on, and its loopback is up;
/// - the facts a namespace can't be reset of, handed on: a UDP datagram A's namespace sent is still in its
///   counters for B, and B can tell it was A's (the same inode);
/// - each guard, B fresh (another inode) and the reason counted once more: a TCP connection in TIME_WAIT,
///   a socket still open (held from outside), an address added, a route added;
/// - A holding CAP_NET_ADMIN, A with `/proc/sys` writable, A of no owner: never kept, each counted;
/// - B of another owner: fresh, counted Owner, A's kept for O and handed to O's next box.
fn a_boxs_network_namespace_is_reused_within_its_owner_when_clean(lab: &Lab, own: &Path) {
    if lab.rootless {
        eprintln!("SKIP netns reuse: a rootless node keeps no network namespaces");
        return;
    }
    let (node, _state) = lab.node(&own.join("hex-reuse-netns"));
    let o = BoxOwner::named("owner-net").unwrap();
    let wait = ["/hex", "selftest-box", "wait"];
    let make = |id: &str, owner: Option<&BoxOwner>, caps: Caps, readonly_sys: bool| BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: BoxId::new(id).unwrap(),
        bounds: Bounds::default(),
        class: ClassSpec::Bundle(lab.bundle_with(caps, &wait, true, readonly_sys)),
        owner: owner.cloned(),
    };
    let counts = || {
        node.netns_reuse()
            .unwrap()
            .expect("a root node keeps namespaces")
    };
    let netns_of = |id: &str| -> PathBuf {
        let cg = node.cgroup_dir(&BoxId::new(id).unwrap()).unwrap();
        let procs = std::fs::read_to_string(cg.join("cgroup.procs")).unwrap();
        let pid = procs.lines().next().expect("the box's init").to_owned();
        std::fs::read_link(format!("/proc/{pid}/ns/net")).unwrap()
    };
    let init_of = |id: &str| -> String {
        let cg = node.cgroup_dir(&BoxId::new(id).unwrap()).unwrap();
        let procs = std::fs::read_to_string(cg.join("cgroup.procs")).unwrap();
        procs.lines().next().expect("the box's init").to_owned()
    };
    // A namespace's inode is its identity only while it lives: one freed may have its number given to the next
    // one made. So each comparison holds the earlier box's namespace open until it's made.
    let hold = |id: &str| std::fs::File::open(format!("/proc/{}/ns/net", init_of(id))).unwrap();
    // The node's checker thread rules on each namespace given back, in turn: the fact to wait on is its
    // ruling on every one given back so far, the one just given back among them.
    let ruled = || {
        wait_for(
            &CHECKED,
            || {
                let c = counts();
                c.checked + c.unchecked == c.given_back
            },
            "the checker's ruling on every namespace given back",
        );
        counts()
    };

    // Handed on: B gets A's namespace, and its loopback is up.
    node.create(make("net-a", Some(&o), Caps::Docker, true))
        .unwrap();
    let a_ns = netns_of("net-a");
    let a_held = hold("net-a");
    in_netns(&init_of("net-a"), || {
        let s = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        s.send_to(b"x", s.local_addr().unwrap()).unwrap();
    });
    let before = ruled();
    node.delete(&BoxId::new("net-a").unwrap()).unwrap();
    let after = ruled();
    assert_eq!(after.given_back, before.given_back + 1, "{after:?}");
    assert_eq!(
        after.pool.idle,
        before.pool.idle + 1,
        "A's namespace is kept for O: {after:?}"
    );
    let before = counts();
    node.create(make("net-b", Some(&o), Caps::Docker, true))
        .unwrap();
    assert_eq!(
        netns_of("net-b"),
        a_ns,
        "B is given A's namespace, and can tell"
    );
    assert_eq!(counts().pool.handed_on, before.pool.handed_on + 1);
    let snmp = std::fs::read_to_string(format!("/proc/{}/net/snmp", init_of("net-b"))).unwrap();
    assert!(
        udp_out(&snmp) >= 1,
        "A's datagram is still in the namespace's counters for B: {snmp}"
    );
    in_netns(&init_of("net-b"), || {
        std::net::TcpListener::bind("127.0.0.1:0").expect("B's loopback is up");
    });
    drop(a_held);
    eprintln!("netns: B given A's namespace, counted, its loopback up, A's counters still in it");
    node.delete(&BoxId::new("net-b").unwrap()).unwrap();
    ruled();
    node.release_idle().unwrap();

    // Each guard: what A's namespace holds at its delete refuses it, by why, and B gets a fresh one.
    let mut held: Vec<std::net::UdpSocket> = Vec::new();
    for (case, reason) in [
        ("timewait", NetRefused::TimeWait),
        ("socket", NetRefused::Sockets),
        ("address", NetRefused::Addresses),
        ("route", NetRefused::Routes),
    ] {
        let a = format!("net-{case}-a");
        node.create(make(&a, Some(&o), Caps::Docker, true)).unwrap();
        let a_ns = netns_of(&a);
        let a_held = hold(&a);
        let pid = init_of(&a);
        match case {
            "timewait" => in_netns(&pid, time_wait),
            "socket" => {
                let (tx, rx) = std::sync::mpsc::channel();
                in_netns(&pid, || {
                    tx.send(std::net::UdpSocket::bind("127.0.0.1:0").unwrap())
                        .unwrap()
                });
                held.push(rx.recv().unwrap());
            }
            "address" => in_netns(&pid, || rtnetlink(new_address())),
            _ => in_netns(&pid, || rtnetlink(new_route())),
        }
        let before = ruled();
        node.delete(&BoxId::new(&a).unwrap()).unwrap();
        let after = ruled();
        assert_eq!(
            after.pool.refused(reason),
            before.pool.refused(reason) + 1,
            "{case}: counted as {}: {after:?}",
            reason.name()
        );
        if case == "socket" {
            assert_eq!(
                after.protocols,
                before.protocols + 1,
                "{case}: a socket past the check's own reads the protocols, counted: {after:?}"
            );
        }
        assert_eq!(after.pool.idle, before.pool.idle, "{case}: not kept");
        let b = format!("net-{case}-b");
        node.create(make(&b, Some(&o), Caps::Docker, true)).unwrap();
        assert_ne!(netns_of(&b), a_ns, "{case}: B is given a fresh namespace");
        drop(a_held);
        node.delete(&BoxId::new(&b).unwrap()).unwrap();
        ruled();
        node.release_idle().unwrap();
        eprintln!("netns, {case}: A's refused, counted, B fresh");
    }
    drop(held);

    // A socket never hashed (a UDP socket never bound, held outside) is counted by sockstat and by no protocol:
    // the check reads the protocols, counted, and keeps the namespace, as decision 5 says it crosses within the
    // owner (the lead's conditions for the check's own socket, 2026-11-03T17:36Z).
    node.create(make("net-unhashed-a", Some(&o), Caps::Docker, true))
        .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    in_netns(&init_of("net-unhashed-a"), || {
        tx.send(
            rustix::net::socket(
                rustix::net::AddressFamily::INET,
                rustix::net::SocketType::DGRAM,
                None,
            )
            .unwrap(),
        )
        .unwrap()
    });
    let unbound = rx.recv().unwrap();
    let before = ruled();
    node.delete(&BoxId::new("net-unhashed-a").unwrap()).unwrap();
    let after = ruled();
    assert_eq!(
        after.protocols,
        before.protocols + 1,
        "unhashed: the protocols read, counted: {after:?}"
    );
    assert_eq!(
        after.pool.idle,
        before.pool.idle + 1,
        "unhashed: in no protocol's count, so kept for O: {after:?}"
    );
    drop(unbound);
    node.release_idle().unwrap();
    eprintln!("netns, a socket never hashed: the protocols read, counted, the namespace kept");

    // Never kept: a box that could have changed its namespace, or that names no owner.
    for (case, owner, caps, readonly_sys, reason) in [
        (
            "netadmin",
            Some(&o),
            Caps::NetAdmin,
            true,
            NetRefused::NetAdmin,
        ),
        (
            "procsys",
            Some(&o),
            Caps::Docker,
            false,
            NetRefused::ProcSysWritable,
        ),
        ("noowner", None, Caps::Docker, true, NetRefused::NoOwner),
    ] {
        let before = ruled();
        let a = format!("net-{case}");
        node.create(make(&a, owner, caps, readonly_sys)).unwrap();
        assert_eq!(
            counts().pool.refused(reason),
            before.pool.refused(reason) + 1,
            "{case}: counted as {}",
            reason.name()
        );
        node.delete(&BoxId::new(&a).unwrap()).unwrap();
        let after = counts();
        assert_eq!(after.given_back, before.given_back, "{case}: never kept");
        assert_eq!(after.pool.idle, before.pool.idle, "{case}: never kept");
        eprintln!("netns, {case}: a namespace of its own, never kept, counted");
    }

    // Another owner's boxes: each fresh, counted, never O's; and each owner's idle namespaces go to that
    // owner's next box alone, oldest first (Ada's call; the lead, 2026-11-03T06:12Z: a namespace never
    // reaches another owner's box). Every namespace compared is alive, kept idle or a live box's.
    let p = BoxOwner::named("owner-net-p").unwrap();
    node.create(make("net-o-a", Some(&o), Caps::Docker, true))
        .unwrap();
    let a_ns = netns_of("net-o-a");
    node.delete(&BoxId::new("net-o-a").unwrap()).unwrap();
    let before = ruled();
    assert_eq!(
        before.pool.idle, 1,
        "A's namespace is kept for O: {before:?}"
    );
    let mut p_ns = Vec::new();
    for i in 0..3 {
        let id = format!("net-p-{i}");
        node.create(make(&id, Some(&p), Caps::Docker, true))
            .unwrap();
        let ns = netns_of(&id);
        assert_ne!(ns, a_ns, "P's box {i} never gets O's");
        p_ns.push(ns);
    }
    assert_eq!(
        counts().pool.refused(NetRefused::Owner),
        before.pool.refused(NetRefused::Owner) + 3,
        "each of P's boxes counted beside O's idle one"
    );
    for i in 0..3 {
        node.delete(&BoxId::new(&format!("net-p-{i}")).unwrap())
            .unwrap();
    }
    let after = ruled();
    assert_eq!(
        after.pool.idle, 4,
        "O's one and P's three, each kept for its owner: {after:?}"
    );
    node.create(make("net-o-c", Some(&o), Caps::Docker, true))
        .unwrap();
    assert_eq!(
        netns_of("net-o-c"),
        a_ns,
        "O's next box gets O's, never one of P's"
    );
    node.create(make("net-p-3", Some(&p), Caps::Docker, true))
        .unwrap();
    assert_eq!(netns_of("net-p-3"), p_ns[0], "P's next box gets P's oldest");
    eprintln!(
        "netns, another owner: P's boxes fresh, counted, never O's; each owner's next box given its own owner's"
    );
    for id in ["net-o-c", "net-p-3"] {
        node.delete(&BoxId::new(id).unwrap()).unwrap();
    }
    ruled();
    node.release_idle().unwrap();
}

/// Goal 5 extended, step 6 (a kept namespace's `/proc/net` files, so a later check needn't enter it; the lead's
/// conditions: closed on every path, counted; CLAUDE.md §2). Do: give the node a soft descriptor limit with room for
/// the namespaces it keeps, as an installer would, keep one owner's namespace, reuse it, leave an
/// address in it, a socket in the next, release what's idle, give one past the owner's bound, take and give back
/// each of the bound's again, then let the node go with a box's namespace held and recover over its state, counting
/// the files open in this process by the names their links show, and all its descriptors at the end. Expect: the
/// first clean check enters and opens the four; each later one comes to its verdict through them, a dirty one's
/// too, and one whose files' dentries a later open of their names unhashed, but a socket past its own, which
/// enters; a box's namespace keeps them; a namespace refused, released or put out by the bound closes them, and so
/// does the node's end; a node recovered holds none; and every descriptor is back to the count before the first
/// node.
fn a_kept_namespace_holds_its_proc_net_files_and_they_close_with_it(lab: &Lab, own: &Path) {
    if lab.rootless {
        eprintln!("SKIP held files: a rootless node keeps no network namespaces");
        return;
    }
    // Its sockstat and dev, and each iptables file its host's modules make.
    let files = 2 + ["ip_tables_names", "ip6_tables_names"]
        .iter()
        .filter(|table| Path::new("/proc/net").join(table).exists())
        .count();
    let open_fds = || std::fs::read_dir("/proc/self/fd").unwrap().count();
    let box_root = own.join("hex-reuse-files");
    let (first_node, _first_state) = lab.node(&box_root);
    drop(first_node);
    let before_all = open_fds();
    let (node, state) = lab.node(&box_root);
    // The node's start raises its soft descriptor limit to its hard one and fits its bounds to it, its kept namespaces'
    // files taking what its own leave (hex_node::netns::hold_descriptors): read from the node, as it started, whether
    // that holds the files of the namespaces this case keeps, two owners' idle ones and a few more.
    let per_owner = node.sizes().start;
    let room = u64::try_from(2 * per_owner + 8).unwrap();
    let bound = node
        .netns_reuse()
        .unwrap()
        .expect("a root node keeps namespaces")
        .files_bound
        .expect("a root node's bound");
    if bound.namespaces < room {
        eprintln!(
            "SKIP held files: the hard descriptor limit leaves room for {} namespaces' files past the node's own {}, \
             short of the {room} this case keeps",
            bound.namespaces, bound.own
        );
        return;
    }
    let o = BoxOwner::named("owner-files").unwrap();
    let wait = ["/hex", "selftest-box", "wait"];
    let make = |id: &str| BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: BoxId::new(id).unwrap(),
        bounds: Bounds::default(),
        class: ClassSpec::Bundle(lab.bundle_with(Caps::Docker, &wait, true, true)),
        owner: Some(o.clone()),
    };
    let counts = || {
        node.netns_reuse()
            .unwrap()
            .expect("a root node keeps namespaces")
    };
    let ruled = || {
        wait_for(
            &CHECKED,
            || {
                let c = counts();
                c.checked + c.unchecked == c.given_back
            },
            "the checker's ruling on every namespace given back",
        );
        counts()
    };
    let init_of = |id: &str| -> String {
        let cg = node.cgroup_dir(&BoxId::new(id).unwrap()).unwrap();
        let procs = std::fs::read_to_string(cg.join("cgroup.procs")).unwrap();
        procs.lines().next().expect("the box's init").to_owned()
    };
    let delete = |id: &str| node.delete(&BoxId::new(id).unwrap()).unwrap();
    // A socket counts until it's freed, after a grace period, so a namespace's count may stand past the check's
    // own for a while, and a check then enters for the protocols. Waited out where a case asks for the files.
    let settled = |id: &str| {
        let sockstat = format!("/proc/{}/net/sockstat", init_of(id));
        wait_for(
            &SOCKETS,
            || {
                std::fs::read_to_string(&sockstat)
                    .unwrap()
                    .starts_with("sockets: used 1\n")
            },
            "the namespace's count down to the check's own socket",
        );
    };
    // A file of `/proc/thread-self/net` opened links to its name under the thread's directory, and reads
    // " (deleted)" once the same thread opens that name again: a `/proc/net` name is never revalidated (Linux
    // v6.19 fs/proc/generic.c:350-354), so the later lookup unhashes the dentry the held file stands on
    // (fs/namei.c:1715-1718) and its path is told as deleted (fs/d_path.c:287-288). The file still reads its
    // namespace, which it took as it opened, so it is counted either way.
    let held = || {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|fd| std::fs::read_link(fd.ok()?.path()).ok())
            .filter(|link| {
                let link = link.to_string_lossy();
                let link = link.strip_suffix(" (deleted)").unwrap_or(&link);
                ["sockstat", "dev", "ip_tables_names", "ip6_tables_names"]
                    .iter()
                    .any(|name| link.ends_with(&format!("/net/{name}")))
            })
            .count()
    };
    assert_eq!(held(), 0, "no namespace kept yet");

    node.create(make("files-a")).unwrap();
    let start = ruled();
    delete("files-a");
    let first = ruled();
    assert_eq!(first.pool.idle, start.pool.idle + 1, "{first:?}");
    assert_eq!(
        first.through_files, start.through_files,
        "the first check enters: {first:?}"
    );
    assert_eq!(first.files_failed, 0, "{first:?}");
    assert_eq!(held(), files, "the clean namespace holds its files");

    node.create(make("files-b")).unwrap();
    assert_eq!(held(), files, "a box's namespace keeps them while it lives");
    settled("files-b");
    delete("files-b");
    let second = ruled();
    assert_eq!(
        second.through_files,
        first.through_files + 1,
        "through its files: {second:?}"
    );
    assert_eq!(second.pool.idle, first.pool.idle, "kept again: {second:?}");
    assert_eq!(held(), files);

    node.create(make("files-c")).unwrap();
    in_netns(&init_of("files-c"), || rtnetlink(new_address()));
    settled("files-c");
    delete("files-c");
    let third = ruled();
    assert_eq!(
        third.through_files,
        second.through_files + 1,
        "through its files: {third:?}"
    );
    assert_eq!(
        third.pool.refused(NetRefused::Addresses),
        second.pool.refused(NetRefused::Addresses) + 1,
        "an address the files' path refuses: {third:?}"
    );
    assert_eq!(held(), 0, "a namespace refused closes them");

    node.create(make("files-d")).unwrap();
    delete("files-d");
    ruled();
    node.create(make("files-e")).unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    in_netns(&init_of("files-e"), || {
        tx.send(std::net::UdpSocket::bind("127.0.0.1:0").unwrap())
            .unwrap()
    });
    let socket = rx.recv().unwrap();
    let before = ruled();
    delete("files-e");
    let after = ruled();
    assert_eq!(
        after.through_files, before.through_files,
        "a socket past its own enters: {after:?}"
    );
    assert_eq!(after.protocols, before.protocols + 1, "{after:?}");
    assert_eq!(
        after.pool.refused(NetRefused::Sockets),
        before.pool.refused(NetRefused::Sockets) + 1,
        "{after:?}"
    );
    drop(socket);
    assert_eq!(held(), 0, "refused by way of entering, it closes them too");

    node.create(make("files-f")).unwrap();
    delete("files-f");
    ruled();
    assert_eq!(held(), files);
    node.release_idle().unwrap();
    wait_for(
        &RELEASED,
        || held() == 0,
        "the files of a namespace released closing",
    );

    // One past the owner's bound, each namespace with its files: the one put out closes its own.
    let many: Vec<String> = (0..=per_owner).map(|n| format!("files-many-{n}")).collect();
    for id in &many {
        node.create(make(id)).unwrap();
    }
    for id in &many {
        delete(id);
    }
    let filled = ruled();
    assert_eq!(
        held(),
        files * per_owner,
        "the namespace the owner's bound put out closed its files: {filled:?}"
    );
    // A checker's directory keeps one hashed dentry a name, so most of the bound's files stand on dentries a later
    // open unhashed (above). Each kept namespace taken by a box and given back is checked through its files all the
    // same.
    let again: Vec<String> = (0..per_owner).map(|n| format!("files-again-{n}")).collect();
    for id in &again {
        node.create(make(id)).unwrap();
    }
    for id in &again {
        settled(id);
    }
    let taken = ruled();
    for id in &again {
        delete(id);
    }
    let checked = ruled();
    assert_eq!(
        checked.through_files,
        taken.through_files + u64::try_from(again.len()).unwrap(),
        "each through its files: {checked:?}"
    );
    assert_eq!(checked.files_failed, 0, "{checked:?}");
    assert_eq!(held(), files * per_owner);
    node.release_idle().unwrap();
    wait_for(
        &RELEASED,
        || held() == 0,
        "the bound's namespaces released closing",
    );
    assert_eq!(counts().files_failed, 0);

    // The node let go with a box's namespace held, its files among what it holds: they close with it, the box runs
    // on, and a node recovered over the same state holds none of them again.
    node.create(make("files-g")).unwrap();
    delete("files-g");
    ruled();
    node.create(make("files-h")).unwrap();
    assert_eq!(held(), files, "the box's namespace holds them");
    drop(node);
    wait_for(&RELEASED, || held() == 0, "the files closing with the node");
    let restarted = Node::open(&box_root, state.path(), state.path())
        .expect("reopen the node")
        .with_oci_helper(PathBuf::from(&lab.exe));
    restarted.recover().expect("recover");
    assert_eq!(held(), 0, "a node recovered holds no namespace's files");
    restarted.delete(&BoxId::new("files-h").unwrap()).unwrap();
    assert_eq!(held(), 0);
    drop(restarted);
    wait_for(
        &RELEASED,
        || open_fds() <= before_all,
        "every descriptor the nodes held closing",
    );
    eprintln!(
        "netns: a kept namespace's files held across a reuse, closed when refused, released, put out or let go"
    );
}

/// Runs [`a_node_past_its_files_bound_checks_by_entering_counted`] in a copy of this binary alone in a fresh cgroup
/// beside this process's leaf, as its own delegated cgroup, since the case lowers its hard descriptor limit. Expect
/// the copy to pass, or to say why it skipped.
fn the_files_bound_case_in_a_copy(lab: &Lab, own: &Path) {
    if lab.rootless {
        eprintln!("SKIP held files' bound: a rootless node keeps no network namespaces");
        return;
    }
    let copy = own.join(format!("hex-files-bound-{}", std::process::id()));
    std::fs::create_dir(&copy).expect("a fresh cgroup for the copy");
    let output = Command::new("/bin/sh")
        .args([
            "-e",
            "-c",
            r#"printf '%s' "$$" > "$1/cgroup.procs"; shift; exec "$@""#,
            "hex-files-bound",
        ])
        .arg(&copy)
        .arg(std::env::current_exe().expect("test binary path"))
        .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(FILES_BOUND_ENV, "1")
        .output()
        .expect("run the files' bound case in a copy");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    eprint!("{log}");
    assert!(
        output.status.success(),
        "the files' bound case failed in its copy"
    );
    assert!(
        log.lines()
            .any(|line| line == FILES_BOUND_PASS || line.starts_with("SKIP held files' bound")),
        "the files' bound case neither passed nor said why it skipped"
    );
}

/// Goal 5 extended step 6, the held files' bound (the lead, 2026-11-03T23:07Z, 2026-11-04T09:15Z and 2026-11-11;
/// CLAUDE.md §2, §3): a node holds kept namespaces' files for at most what its hard descriptor limit, to which its start
/// raises its soft one, leaves past its own descriptors at the bounds its start fitted, four a namespace. Run alone in
/// a copy of this binary ([`the_files_bound_case_in_a_copy`]). Do: open a node and read its bound and its pools; lower
/// this process's hard limit to that node's own and two namespaces' files; open a node again, give back four boxes'
/// namespaces of one owner, take and give back all four again, release them, and let the node go. Expect: the second
/// node's pools the first's and its bound two, the limit as it was set; two namespaces hold their files and two don't,
/// whose checks entered, counted; in the second round two checks through files and two more entered, counted; none
/// failed; after the release no file held; and every descriptor back to the count before the node.
fn a_node_past_its_files_bound_checks_by_entering_counted(lab: &Lab, own: &Path) {
    use hex_node::netns::FilesBound;
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};

    if lab.rootless {
        eprintln!("SKIP held files' bound: a rootless node keeps no network namespaces");
        return;
    }
    let files = 2 + ["ip_tables_names", "ip6_tables_names"]
        .iter()
        .filter(|table| Path::new("/proc/net").join(table).exists())
        .count();
    let held = || {
        std::fs::read_dir("/proc/self/fd")
            .unwrap()
            .filter_map(|fd| std::fs::read_link(fd.ok()?.path()).ok())
            .filter(|link| {
                let link = link.to_string_lossy();
                let link = link.strip_suffix(" (deleted)").unwrap_or(&link);
                ["sockstat", "dev", "ip_tables_names", "ip6_tables_names"]
                    .iter()
                    .any(|name| link.ends_with(&format!("/net/{name}")))
            })
            .count()
    };
    let open_fds = || std::fs::read_dir("/proc/self/fd").unwrap().count();
    let box_root = own.join("hex-reuse-files-bound");
    let before_warm = open_fds();
    let (first_node, _first_state) = lab.node(&box_root);
    // The bound and the pools as the node's start derived them, beside what it held as it took them.
    let warm = first_node
        .netns_reuse()
        .unwrap()
        .expect("a root node keeps namespaces")
        .files_bound
        .expect("a root node's bound");
    let pools = first_node.pools();
    // Its start fits its bounds down only under a hard limit short of its own at its host's bounds, where none holds
    // those bounds and exactly two namespaces' files.
    let fitted_down = first_node.sizes().boxes < hex_runtime::MAX_BOXES;
    let unlimited = FilesBound::of(
        &Rlimit {
            current: None,
            maximum: None,
        },
        pools,
        0,
    );
    // Both pure APIs count the same kept namespaces; their ratio is the policy's file charge for one.
    let files_a_kept = FilesBound::needed(pools, 0)
        .files
        .checked_div(unlimited.namespaces)
        .expect("a root node has kept namespace capacity");
    let limit = warm.own + 2 * files_a_kept;
    drop(first_node);
    // The supervisor's weak watching thread releases its epoll after it observes the node's drop.
    // Wait for that descriptor too, so the next startup is measured beside the same live descriptors.
    wait_for(
        &RELEASED,
        || open_fds() <= before_warm,
        "the warm node's descriptors closing",
    );
    let given = getrlimit(Resource::Nofile);
    if fitted_down || given.maximum.is_some_and(|hard| hard < limit) {
        eprintln!(
            "SKIP held files' bound: the hard descriptor limit {:?} is short of the node's own at its host's bounds \
             and two namespaces' files, {limit}",
            given.maximum
        );
        return;
    }
    // The node's start raises its soft limit to this hard one and keeps its bounds, which fit it, so its files take
    // what its own leave: two namespaces'. This copy runs no case after it.
    setrlimit(
        Resource::Nofile,
        Rlimit {
            current: Some(limit),
            maximum: Some(limit),
        },
    )
    .expect("lowering the hard descriptor limit");
    let before_all = open_fds();
    let (node, _state) = lab.node(&box_root);
    let o = BoxOwner::named("owner-files-bound").unwrap();
    let wait = ["/hex", "selftest-box", "wait"];
    let make = |id: &str| BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: BoxId::new(id).unwrap(),
        bounds: Bounds::default(),
        class: ClassSpec::Bundle(lab.bundle_with(Caps::Docker, &wait, true, true)),
        owner: Some(o.clone()),
    };
    let counts = || {
        node.netns_reuse()
            .unwrap()
            .expect("a root node keeps namespaces")
    };
    let ruled = || {
        wait_for(
            &CHECKED,
            || {
                let c = counts();
                c.checked + c.unchecked == c.given_back
            },
            "the checker's ruling on every namespace given back",
        );
        counts()
    };
    let start = counts();
    let bound = start.files_bound.expect("a root node's bound");
    assert_eq!(
        node.pools(),
        pools,
        "the node's start kept its bounds under the limit"
    );
    assert_eq!(bound.namespaces, 2, "{bound:?}");
    assert_eq!(
        bound.soft,
        Some(limit),
        "the limit as it was set, the hard one its soft one was raised to"
    );
    assert_eq!(getrlimit(Resource::Nofile).current, Some(limit));
    let ids: Vec<String> = (0..4).map(|n| format!("files-bound-{n}")).collect();
    for round in 0..2 {
        // Each namespace's count settled before its check: a fresh one's (round 0) at none once its loopback
        // request's socket is freed after a grace period (net/netlink/af_netlink.c:794-796, :703-715), a kept one's
        // (round 1) at the check's own socket. Each check then reads `sockstat`'s count alone.
        let settled = if round == 0 {
            "sockets: used 0\n"
        } else {
            "sockets: used 1\n"
        };
        for id in &ids {
            node.create(make(id)).unwrap();
        }
        for id in &ids {
            let sockstat = format!(
                "/proc/{}/net/sockstat",
                std::fs::read_to_string(
                    node.cgroup_dir(&BoxId::new(id).unwrap())
                        .unwrap()
                        .join("cgroup.procs")
                )
                .unwrap()
                .lines()
                .next()
                .expect("the box's init")
            );
            wait_for(
                &SETTLED,
                || {
                    std::fs::read_to_string(&sockstat)
                        .unwrap()
                        .starts_with(settled)
                },
                "the namespace's count settled",
            );
        }
        let before = ruled();
        for id in &ids {
            node.delete(&BoxId::new(id).unwrap()).unwrap();
        }
        let after = ruled();
        assert_eq!(after.files_held, 2, "round {round}: {after:?}");
        assert_eq!(held(), 2 * files, "round {round}: {after:?}");
        assert_eq!(
            after.files_bounded,
            before.files_bounded + 2,
            "round {round}: two past the bound entered, counted: {after:?}"
        );
        assert_eq!(
            after.through_files,
            before.through_files + if round == 0 { 0 } else { 2 },
            "round {round}: {after:?}"
        );
        assert_eq!(after.files_failed, 0, "{after:?}");
    }
    node.release_idle().unwrap();
    wait_for(
        &RELEASED,
        || held() == 0,
        "the namespaces released closing their files",
    );
    assert_eq!(counts().files_held, 0);
    drop(node);
    wait_for(
        &RELEASED,
        || open_fds() <= before_all,
        "every descriptor the node held let go",
    );
    eprintln!("{FILES_BOUND_PASS}");
}

/// Goal 5 extended, step 6 (the check's own socket, kept in each namespace it checks; the lead's conditions,
/// 2026-11-03T17:36Z): a box given a namespace the node checked sees the node's netlink socket in it and can't
/// send to it. Root only, as the namespace's reuse. Do: A of O deleted, its namespace checked and kept; B of O
/// given it; from B, an rtnetlink request to the route socket with a portid in B's namespace, then one to the
/// kernel; B deleted. Expect: B's `/proc/net/netlink` lists one route socket with a portid, this process's (the
/// node agent's) by its inode; B's send to it refused with EPERM, since NETLINK_ROUTE takes a message to another
/// socket only from one with CAP_NET_ADMIN over the namespace's owner (Linux v6.19 net/core/rtnetlink.c:7038;
/// net/netlink/af_netlink.c:1844-1846, :904-908), and B's send to the kernel gone (non-vacuity); the socket's
/// queue and drops 0 before and after; and the namespace kept again at its next check.
fn a_box_can_not_send_to_the_socket_the_check_keeps_in_its_namespace(lab: &Lab, own: &Path) {
    if lab.rootless {
        eprintln!("SKIP netns kept socket: a rootless node keeps no network namespaces");
        return;
    }
    let (node, _state) = lab.node(&own.join("hex-reuse-netsock"));
    let o = BoxOwner::named("owner-netsock").unwrap();
    let wait = ["/hex", "selftest-box", "wait"];
    let make = |id: &str| BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: BoxId::new(id).unwrap(),
        bounds: Bounds::default(),
        class: ClassSpec::Bundle(lab.bundle_with(Caps::Docker, &wait, true, true)),
        owner: Some(o.clone()),
    };
    let counts = || {
        node.netns_reuse()
            .unwrap()
            .expect("a root node keeps namespaces")
    };
    let ruled = || {
        wait_for(
            &CHECKED,
            || {
                let c = counts();
                c.checked + c.unchecked == c.given_back
            },
            "the checker's ruling on every namespace given back",
        );
        counts()
    };
    node.create(make("sock-a")).unwrap();
    node.delete(&BoxId::new("sock-a").unwrap()).unwrap();
    let kept = ruled();
    assert_eq!(
        kept.pool.idle, 1,
        "A's namespace is checked and kept for O: {kept:?}"
    );
    node.create(make("sock-b")).unwrap();
    assert_eq!(
        counts().pool.handed_on,
        kept.pool.handed_on + 1,
        "B is given A's namespace"
    );
    let b = BoxId::new("sock-b").unwrap();
    let procs = std::fs::read_to_string(node.cgroup_dir(&b).unwrap().join("cgroup.procs")).unwrap();
    let init = procs.lines().next().expect("B's init").to_owned();
    let route_sockets = || -> Vec<NetlinkSocket> {
        netlink_sockets(&init)
            .into_iter()
            .filter(|s| s.protocol == 0 && s.portid != 0)
            .collect()
    };
    let before = route_sockets();
    assert_eq!(
        before.len(),
        1,
        "B's namespace holds one route socket with a portid: {before:?}"
    );
    let socket = before[0];
    let theirs = format!("socket:[{}]", socket.inode);
    assert!(
        std::fs::read_dir("/proc/self/fd").unwrap().any(|fd| {
            std::fs::read_link(fd.unwrap().path()).is_ok_and(|to| to.as_os_str() == theirs.as_str())
        }),
        "the socket at portid {} is this process's, the node agent's",
        socket.portid
    );
    assert_eq!((socket.rmem, socket.drops), (0, 0), "{socket:?}");
    let portid = socket.portid.to_string();
    assert_eq!(
        run(
            &node,
            &b,
            Program::of(&["/hex", "selftest-box", "netlink-to", &portid]),
            &SEND_ENDS,
        ),
        Exit::Code(1),
        "B's send to the node's socket is refused, EPERM"
    );
    assert_eq!(
        run(
            &node,
            &b,
            Program::of(&["/hex", "selftest-box", "netlink-to", "0"]),
            &SEND_ENDS,
        ),
        Exit::Code(0),
        "B's send to the kernel goes"
    );
    assert_eq!(
        route_sockets(),
        before,
        "the node's socket as it was: nothing queued, nothing dropped"
    );
    node.delete(&b).unwrap();
    let after = ruled();
    assert_eq!(
        after.pool.idle, 1,
        "the namespace passes its next check, kept for O: {after:?}"
    );
    eprintln!(
        "netns, kept socket: B sees the node's at portid {portid}, its send to it refused (EPERM), its send to the kernel gone, the socket unchanged, the namespace kept again"
    );
    node.release_idle().unwrap();
}

/// A netlink socket as `/proc/<pid>/net/netlink` lists it, in that process's network namespace (Linux v6.19
/// net/netlink/af_netlink.c:2703, `netlink_native_seq_show`: sk, Eth, Pid, Groups, Rmem, Wmem, Dump, Locks, Drops,
/// Inode): its protocol, its portid, the bytes queued for it to read, its drops and its inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct NetlinkSocket {
    protocol: u32,
    portid: u32,
    rmem: u64,
    drops: u64,
    inode: u64,
}

fn netlink_sockets(pid: &str) -> Vec<NetlinkSocket> {
    std::fs::read_to_string(format!("/proc/{pid}/net/netlink"))
        .unwrap()
        .lines()
        .skip(1)
        .map(|line| {
            let f: Vec<&str> = line.split_whitespace().collect();
            NetlinkSocket {
                protocol: f[1].parse().unwrap(),
                portid: f[2].parse().unwrap(),
                rmem: f[4].parse().unwrap(),
                drops: f[8].parse().unwrap(),
                inode: f[9].parse().unwrap(),
            }
        })
        .collect()
}

/// Runs `f` on a thread of its own in process `pid`'s network namespace, and waits for it.
fn in_netns(pid: &str, f: impl FnOnce() + Send) {
    let ns = std::fs::File::open(format!("/proc/{pid}/ns/net")).unwrap();
    std::thread::scope(|scope| {
        scope
            .spawn(move || {
                rustix::thread::move_into_link_name_space(
                    std::os::fd::AsFd::as_fd(&ns),
                    Some(rustix::thread::LinkNameSpaceType::Network),
                )
                .unwrap();
                f();
            })
            .join()
            .unwrap();
    });
}

/// A TCP connection on loopback whose client closes first, so it lingers in TIME_WAIT
/// (include/net/tcp.h:141, 60 s).
fn time_wait() {
    use std::io::Read as _;
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let client = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    let (mut server, _) = listener.accept().unwrap();
    client.shutdown(std::net::Shutdown::Both).unwrap();
    drop(client);
    // The server reads the client's end, then closes its own: the client's side is left in TIME_WAIT.
    let mut sink = Vec::new();
    let _ = server.read_to_end(&mut sink);
    drop(server);
}

/// Udp's OutDatagrams in a `/proc/net/snmp`: its "Udp:" lines, names then values.
fn udp_out(snmp: &str) -> u64 {
    let mut udp = snmp.lines().filter(|l| l.starts_with("Udp:"));
    let (names, values) = (udp.next().unwrap(), udp.next().unwrap());
    let at = names
        .split_whitespace()
        .position(|n| n == "OutDatagrams")
        .unwrap();
    values.split_whitespace().nth(at).unwrap().parse().unwrap()
}

/// Sends one rtnetlink request on a socket of the calling thread's namespace and wants its ack.
fn rtnetlink(request: Vec<u8>) {
    use rustix::net::{AddressFamily, RecvFlags, SendFlags, SocketFlags, SocketType};
    let fd = rustix::net::socket_with(
        AddressFamily::NETLINK,
        SocketType::RAW,
        SocketFlags::CLOEXEC,
        None,
    )
    .unwrap();
    rustix::net::send(&fd, &request, SendFlags::empty()).unwrap();
    let mut answer = [0u8; 1024];
    let (got, _) = rustix::net::recv(&fd, &mut answer, RecvFlags::empty()).unwrap();
    let error = i32::from_ne_bytes(answer[16..20].try_into().unwrap());
    assert!(got >= 20 && error == 0, "the kernel refused it: {error}");
}

/// A netlink request of `kind` (`NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL`) with `body`.
fn nl_request(kind: u16, body: &[u8]) -> Vec<u8> {
    let mut m = Vec::new();
    m.extend_from_slice(&u32::try_from(16 + body.len()).unwrap().to_ne_bytes());
    m.extend_from_slice(&kind.to_ne_bytes());
    m.extend_from_slice(&(0x1u16 | 0x4 | 0x400 | 0x200).to_ne_bytes());
    m.extend_from_slice(&1u32.to_ne_bytes());
    m.extend_from_slice(&0u32.to_ne_bytes());
    m.extend_from_slice(body);
    m
}

fn nl_attr(kind: u16, value: &[u8]) -> Vec<u8> {
    let mut a = Vec::new();
    a.extend_from_slice(&u16::try_from(4 + value.len()).unwrap().to_ne_bytes());
    a.extend_from_slice(&kind.to_ne_bytes());
    a.extend_from_slice(value);
    while a.len() % 4 != 0 {
        a.push(0);
    }
    a
}

/// `RTM_NEWADDR` (20): 10.11.12.13/32 on loopback (interface 1), `IFA_LOCAL` (2) and `IFA_ADDRESS` (1).
fn new_address() -> Vec<u8> {
    let mut body = vec![2u8, 32, 0, 0];
    body.extend_from_slice(&1u32.to_ne_bytes());
    body.extend(nl_attr(2, &[10, 11, 12, 13]));
    body.extend(nl_attr(1, &[10, 11, 12, 13]));
    nl_request(20, &body)
}

/// `RTM_NEWROUTE` (24): 10.20.0.0/16 out of loopback in the main table (254), unicast, link scope.
fn new_route() -> Vec<u8> {
    // struct rtmsg: family, dst_len, src_len, tos, table, protocol (static, 4), scope (link, 253),
    // type (unicast, 1), flags.
    let mut body = vec![2u8, 16, 0, 0, 254, 4, 253, 1];
    body.extend_from_slice(&0u32.to_ne_bytes());
    body.extend(nl_attr(1, &[10, 20, 0, 0]));
    body.extend(nl_attr(4, &1u32.to_ne_bytes()));
    nl_request(24, &body)
}

/// Waits for `cond` on `wait`, its own fact's record or measuring (hex_testkit::waits): says how many looks it took.
fn wait_for(wait: &Patience, cond: impl FnMut() -> bool, what: &str) -> u64 {
    until(wait, what, cond).unwrap()
}

// Each wait here takes its own fact's record (hex_testkit::waits; the lead's rule of 2026-11-08), or measures where no
// recorded operation makes its fact, its answers recorded for the census to come, from the suites run under it.
/// A killed box's processes ending: `node.emptying`.
static KILLED: Patience = Patience::seeded("test.oci_reuse.killed", EMPTYING);
/// An init's run to its end: the program-run record.
static INIT_ENDS: Patience = Patience::seeded("test.oci_reuse.init_ends", PROGRAM);
/// An exec of `true` run to its end: the program-run record's own fact (`lifecycle.exec`, an exec of `true`).
static TRUE_ENDS: Patience = Patience::seeded("test.oci_reuse.true_ends", PROGRAM);
/// A 3 GB sha256 burn's end, a 64 MiB hog's, a 1 MiB hog's, a hog's OOM kill past its bound, a spawn's refusal past
/// its bound, and 64 MiB written to a box's root: none is an exec of `true`, whose end the program-run record is (each
/// runs its own work first), and no recorded operation makes any.
static BURN_ENDS: Patience = Patience::measuring("test.oci_reuse.burn_ends");
static HOG_ENDS: Patience = Patience::measuring("test.oci_reuse.hog_ends");
static SMALL_HOG_ENDS: Patience = Patience::measuring("test.oci_reuse.small_hog_ends");
static OOM_ENDS: Patience = Patience::measuring("test.oci_reuse.oom_ends");
static REFUSED_ENDS: Patience = Patience::measuring("test.oci_reuse.refused_ends");
static WRITE_ENDS: Patience = Patience::measuring("test.oci_reuse.write_ends");
/// A box's netlink send's end, refused or taken: the hex binary run in the box to its own socket's send, not an exec of
/// `true`'s, and made by no recorded operation.
static SEND_ENDS: Patience = Patience::measuring("test.oci_reuse.send_ends");
/// An owner's idle slot given its parked init: made within the delete (`Cgroups::give_back`), whose record bounds it.
static PARKED: Patience = Patience::seeded("test.oci_reuse.parked", DELETE);
/// A parked init asleep for its claim once its delete has answered: its own steps after it said it is parked, which
/// no recorded operation makes.
static PARKED_ASLEEP: Patience = Patience::measuring("test.oci_reuse.parked_asleep");
/// Trees queued to the parked procs' dropper, dropped by its thread: no recorded operation makes it.
static DROPPED: Patience = Patience::measuring("test.oci_reuse.dropped");
/// A cgroup made ahead by the cgroups' refill thread: no recorded operation makes it.
static MADE_AHEAD: Patience = Patience::measuring("test.oci_reuse.made_ahead");
/// A refused idle cgroup removed by the cgroups' remover thread, off the create's path: no recorded operation makes it.
static REMOVED: Patience = Patience::measuring("test.oci_reuse.removed");
/// The network check's ruling on every namespace given back, by its checker threads: no recorded operation makes it.
static CHECKED: Patience = Patience::measuring("test.oci_reuse.checked");
/// A kept namespace's sockets down to the check's own, and its count settled: no recorded operation makes either.
static SOCKETS: Patience = Patience::measuring("test.oci_reuse.sockets");
static SETTLED: Patience = Patience::measuring("test.oci_reuse.settled");
/// Namespaces' files and the nodes' descriptors released as they close: no recorded operation makes it.
static RELEASED: Patience = Patience::measuring("test.oci_reuse.released");

/// One state guard's case: A of owner O made, started, given `work`, deleted; `leave` done to its idle
/// cgroup; B of O created. Expect B a fresh cgroup, A's removed, `reason` counted once more.
fn a_state_guard(
    lab: &Lab,
    node: &Node,
    case: &str,
    work: Option<(Program, &Patience)>,
    leave: impl FnOnce(&Path),
    reason: Refused,
) {
    let o = BoxOwner::named("owner-guard").unwrap();
    let a = BoxId::new(&format!("{case}-a")).unwrap();
    node.create(lab.oci(
        &format!("{case}-a"),
        Some(&o),
        Caps::Docker,
        Bounds::default(),
    ))
    .unwrap();
    node.start(&a).unwrap();
    if let Some((work, wait)) = work {
        assert_eq!(run(node, &a, work, wait), Exit::Code(0), "{case}: A's work");
    }
    let slot = node.cgroup_dir(&a).unwrap();
    node.delete(&a).unwrap();
    assert!(slot.exists(), "{case}: A's cgroup is idle");
    leave(&slot);
    let before = node.cgroup_reuse().unwrap();
    let b = BoxId::new(&format!("{case}-b")).unwrap();
    node.create(lab.oci(
        &format!("{case}-b"),
        Some(&o),
        Caps::Docker,
        Bounds::default(),
    ))
    .unwrap();
    let after = node.cgroup_reuse().unwrap();
    assert_ne!(
        node.cgroup_dir(&b).as_deref(),
        Some(slot.as_path()),
        "{case}: B is given a fresh cgroup"
    );
    assert_eq!(
        after.pool.refused(reason),
        before.pool.refused(reason) + 1,
        "{case}: counted as {}: {after:?}",
        reason.name()
    );
    wait_for(
        &REMOVED,
        || !slot.exists(),
        &format!("{case}: A's cgroup removed"),
    );
    node.delete(&b).unwrap();
    eprintln!(
        "test 3, {case}: B fresh, A's removed, counted {}",
        reason.name()
    );
}

/// Test 3. Do: for each guard, leave A's idle cgroup unclean, or make A or B a box that may not share
/// one. Expect: each B a fresh cgroup, each refusal counted by its reason, A's removed where it was left
/// unclean or never kept, and an idle cgroup handed to its own owner's box alone.
fn each_guard_gives_a_fresh_cgroup_and_counts_why(lab: &Lab, own: &Path) {
    let (node, _state) = lab.node(&own.join("hex-reuse-guard"));
    a_state_guard(
        lab,
        &node,
        "sub",
        None,
        |slot| std::fs::create_dir(slot.join("left")).unwrap(),
        Refused::Descendants,
    );
    a_state_guard(
        lab,
        &node,
        "oomgroup",
        None,
        |slot| std::fs::write(slot.join("memory.oom.group"), "1").unwrap(),
        Refused::OomGroup,
    );
    let mut outside = Command::new("sleep").arg("600").spawn().expect("sleep");
    let pid = outside.id();
    let outside_fd = pidfd(pid);
    a_state_guard(
        lab,
        &node,
        "live",
        None,
        |slot| std::fs::write(slot.join("cgroup.procs"), pid.to_string()).unwrap(),
        Refused::Populated,
    );
    wait_for(
        &KILLED,
        || has_ended(&outside_fd),
        "the process left in A's cgroup ended with it",
    );
    let _ = outside.wait();
    let big = lab.rootfs.join("big");
    a_state_guard(
        lab,
        &node,
        "charged",
        Some((
            Program::of(&["/bin/sh", "-c", "head -c 67108864 /dev/zero > /big"]),
            &WRITE_ENDS,
        )),
        |slot| {
            let charged: u64 = read(slot, "memory.current").parse().unwrap();
            assert!(charged >= 60 << 20, "A left {charged} bytes charged");
        },
        Refused::Charged,
    );
    std::fs::remove_file(&big).unwrap();

    // A killed through cgroup.kill: its cgroup is never kept, since a task cloned into it later dies.
    let o = BoxOwner::named("owner-guard").unwrap();
    let a = BoxId::new("killed-a").unwrap();
    node.create(lab.oci("killed-a", Some(&o), Caps::Docker, Bounds::default()))
        .unwrap();
    node.start(&a).unwrap();
    let slot = node.cgroup_dir(&a).unwrap();
    node.signal(&a, hex_api::Signal::KILL).unwrap();
    wait_for(
        &KILLED,
        || {
            node.inspect(&a)
                .is_ok_and(|info| info.exit == Some(Exit::Signal(9)))
        },
        "A's init killed",
    );
    let before = node.cgroup_reuse().unwrap();
    node.delete(&a).unwrap();
    let after = node.cgroup_reuse().unwrap();
    assert!(
        !slot.exists(),
        "a cgroup cgroup.kill was written to is removed, not kept: {after:?}"
    );
    assert_eq!(
        after.discarded_killed,
        before.discarded_killed + 1,
        "counted: {after:?}"
    );
    let b = BoxId::new("killed-b").unwrap();
    node.create(lab.oci("killed-b", Some(&o), Caps::Docker, Bounds::default()))
        .unwrap();
    assert_ne!(
        node.cgroup_dir(&b).as_deref(),
        Some(slot.as_path()),
        "B is given a fresh cgroup"
    );
    node.start(&b).unwrap();
    assert_eq!(
        run(&node, &b, selftest(&Act::Hog(1 << 20)), &SMALL_HOG_ENDS),
        Exit::Code(0),
        "B's init and its programs run"
    );
    node.delete(&b).unwrap();
    eprintln!("test 3, killed: A's cgroup removed, not kept, counted; B fresh, and runs");

    // B of another owner: never A's, a fresh cgroup, counted, and A's kept in O's pool for O's next box
    // (Ada's call, 2026-11-03: keep, not the plan's "A's is removed").
    let o = BoxOwner::named("owner-o").unwrap();
    let p = BoxOwner::named("owner-p").unwrap();
    node.create(lab.oci("other-a", Some(&o), Caps::Docker, Bounds::default()))
        .unwrap();
    let slot = node.cgroup_dir(&BoxId::new("other-a").unwrap()).unwrap();
    node.delete(&BoxId::new("other-a").unwrap()).unwrap();
    let before = node.cgroup_reuse().unwrap();
    node.create(lab.oci("other-b", Some(&p), Caps::Docker, Bounds::default()))
        .unwrap();
    let after = node.cgroup_reuse().unwrap();
    assert_ne!(
        node.cgroup_dir(&BoxId::new("other-b").unwrap()).as_deref(),
        Some(slot.as_path()),
        "another owner's box is never given A's cgroup"
    );
    assert_eq!(
        after.pool.refused(Refused::Owner),
        before.pool.refused(Refused::Owner) + 1
    );
    assert!(slot.exists(), "A's cgroup is kept, not removed");
    assert_eq!(
        after.pool.idle, before.pool.idle,
        "A's stays idle in O's pool"
    );
    node.create(lab.oci("other-c", Some(&o), Caps::Docker, Bounds::default()))
        .unwrap();
    assert_eq!(
        node.cgroup_dir(&BoxId::new("other-c").unwrap()).as_deref(),
        Some(slot.as_path()),
        "A's cgroup goes to its own owner's next box"
    );
    eprintln!("test 3, another owner: B fresh, counted, A's kept for O and handed to O's next box");
    for id in ["other-b", "other-c"] {
        node.delete(&BoxId::new(id).unwrap()).unwrap();
    }

    // A of no owner, A holding CAP_SYS_ADMIN, and A whose spec gives no capability sets: a cgroup of
    // its own, gone with it.
    for (case, owner, caps, reason) in [
        ("noowner", None, Caps::Docker, Refused::NoOwner),
        ("sysadmin", Some(&o), Caps::SysAdmin, Refused::Privileged),
        ("unset", Some(&o), Caps::Unset, Refused::Privileged),
    ] {
        let a = BoxId::new(&format!("{case}-a")).unwrap();
        let before = node.cgroup_reuse().unwrap();
        node.create(lab.oci(&format!("{case}-a"), owner, caps, Bounds::default()))
            .unwrap();
        let after = node.cgroup_reuse().unwrap();
        assert_eq!(
            after.pool.refused(reason),
            before.pool.refused(reason) + 1,
            "{case}: counted as {}",
            reason.name()
        );
        let cg = node.cgroup_dir(&a).unwrap();
        assert_eq!(
            cg.file_name().unwrap().to_str().unwrap(),
            a.as_str(),
            "{case}: A's cgroup is its own, named by its id"
        );
        node.delete(&a).unwrap();
        assert!(!cg.exists(), "{case}: A's cgroup is removed with it");
        eprintln!("test 3, A {case}: a cgroup of its own, removed with it, counted");
    }
    // B holding CAP_SYS_ADMIN beside its owner's idle cgroup (other-c's): never handed it.
    let before = node.cgroup_reuse().unwrap();
    assert!(
        before.pool.idle > 0,
        "its owner has an idle cgroup: {before:?}"
    );
    node.create(lab.oci("sysadmin-b", Some(&o), Caps::SysAdmin, Bounds::default()))
        .unwrap();
    let after = node.cgroup_reuse().unwrap();
    assert_eq!(
        after.pool.refused(Refused::Privileged),
        before.pool.refused(Refused::Privileged) + 1
    );
    assert_eq!(
        after.pool.idle, before.pool.idle,
        "the idle cgroup stays its owner's"
    );
    node.delete(&BoxId::new("sysadmin-b").unwrap()).unwrap();
    eprintln!("test 3, B sysadmin: a cgroup of its own, counted, the idle one left");
    node.release_idle().unwrap();
}

/// A rootfs of the host's `/usr`, bound in by each bundle, with the host's top-level links into it
/// (`/bin`, `/lib64`, …) so the loader and `/bin/sh` resolve as on the host.
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
    let mut own = String::new();
    std::fs::File::open("/proc/self/cgroup")
        .and_then(|mut f| f.read_to_string(&mut own))
        .expect("/proc/self/cgroup");
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
        PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/hex-t7/ocireuse");
    std::fs::create_dir_all(&cache).expect("cache dir");
    cache
}
