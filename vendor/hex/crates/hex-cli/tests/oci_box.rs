//! Decision 2's seam half (docs/design/runtime-architecture.md; AC-T7.1.1, T-T7.1.2): through
//! `hex_node::Node`, the node agent is each `oci` box's parent, holds its init's pidfd and records
//! how the init ended. "do X, expect Y" (§6):
//! - a crash: SIGKILL an agent while its box runs, restart it, end the init: the restarted agent
//!   reads the box running, then empty, and nothing of it is left;
//! - create a box: its init's parent is the agent;
//! - a box whose program exits 7: inspect reports `Code(7)` and the init's pid is gone (reaped);
//! - SIGKILL a box's init: inspect reports `Signal(9)`;
//! - past the init budget: create is refused with `Exhausted`, naming it, and leaves nothing;
//! - the hex-api conformance suite on the `oci` class, its exit-report cases included;
//! - decision 4's network namespaces (goal 4 plan step 4): a box whose bundle makes a network
//!   namespace of its own runs in one no other box has had, loopback up. HEX-NETNS-9: from a node whose
//!   pool holds none (bound 0) it is made on the create's path, counted so. HEX-NETNS-8, as root: two
//!   boxes take one each from the node's pool, and the node keeps no descriptor of either.
//!   HEX-NETNS-10, rootless: made on the path in the box's own user namespace, which owns it, nothing
//!   taken;
//! - a box's process orphaned to the agent, once ended, is reaped by the sweep: no zombie is kept;
//! - goal 5 plan step 2: 31 boxes are made at once while another box's durable record write is held
//!   open, none waiting on it, and none of their records synced with their container state on tmpfs;
//!   two nodes with state directories of their own each make and start a box of the same id, two
//!   containers, and deleting one leaves the other running;
//! - goal 5 extended step 5 (T-T7.1.2): a node restarted over records in both its places finds each
//!   box's where it belongs, the boxes whose directories all lie on tmpfs among its runtime directory's
//!   records and one with its rendered bundle's writable layer on disk in its state directory,
//!   fsynced; it takes back the live ones and removes the one whose init ended, and leaves nothing once
//!   they're deleted; a delete of a box while a create of the same id is under way leaves the node
//!   consistent, the create completing and the box then removed, and a second create of a box being made,
//!   or a delete, waits for the create under way (the follow-on fix, §4);
//!
//! Rootless where the caller isn't root, in a delegated cgroup v2 subtree, as oci_seccomp.rs; where
//! neither is possible it records the skip and why (hex_testkit, which the gate prints) rather
//! than pass falsely. The crash runs first, before this process opens any node: an agent's orphans
//! go to its nearest subreaper, and this process becomes one when it opens a node, so a restart in
//! a process that never was the init's ancestor is the real one's.
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
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio as Std};
use std::time::{Duration, Instant};

use hex_api::conformance::{self, Act, Fixture};
use hex_api::{
    Bounds, BoxId, BoxInfo, BoxOwner, BoxSpec, BoxState, ClassSpec, Exit, Network, OciSpec,
    Refusal, Rootfs, Runtime,
};
use hex_core::patience::Patience;
use hex_node::{Node, OCI_BOXES, records_dir};
use hex_runtime::linux::cgroup::BoxRoot;
use hex_runtime::linux::disks;
use hex_runtime::linux::oci::run::ClassicReason;
use hex_testkit::waits::{CREATE, EMPTYING, EXEC, NETNS_MAKE, NODE_TURN, PROGRAM, until};

const TEST: &str = "oci_boxes_report_their_inits_exit_through_the_seam";
/// Runs just the prepared-device oracle through the existing root/delegation fixture, in a fresh
/// process so it cannot become a subreaper before the full suite's agent-crash check.
const PREPARED_DEV_ENV: &str = "HEX_OCI_BOX_PREPARED_DEV";
const PREPARED_DEV_PASS: &str = "prepared /dev oracle completed with actual pooled takes";
const PROPAGATION_ENV: &str = "HEX_OCI_BOX_SHARED_SOURCES";
/// Set by hand to run HEX-MNTNS-8's case alone, as root: a focused run on a kernel that differs (a guest's lab),
/// where a refusal's own text is the record.
const MASKED_DIRS_ENV: &str = "HEX_OCI_BOX_MASKED_DIRS";
/// Runs the rootless command line's cgroup case alone through the lab's fixture, rootless, in a fresh process.
const ROOTLESS_CLI_CGROUP_ENV: &str = "HEX_OCI_BOX_ROOTLESS_CLI_CGROUP";
const ROOTLESS_CLI_CGROUP_PASS: &str =
    "rootless command line: the container's cgroup was made beside the command line's own";
const PROPAGATION_PASS: &str =
    "shared-source propagation oracle completed with four pruned creates";
/// Both entry points can move the test process between cgroups. Serialize their parent fixtures;
/// recover the test-only lock after a failed full suite so the independent oracle still runs.
static LAB_SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());
/// Remember the delegated parent before the first fixture moves this process into its agent leaf.
static LAB_PARENT: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
/// Set in the re-exec'd copy of this test that plays the agent the crash kills.
const AGENT_ENV: &str = "HEX_OCI_BOX_AGENT";

#[test]
fn oci_boxes_report_their_inits_exit_through_the_seam() {
    let exe = env!("CARGO_BIN_EXE_hex").to_owned();
    let cache = cache_dir();
    if let Some(dirs) = std::env::var_os(AGENT_ENV) {
        play_the_agent(Path::new(&dirs), &exe);
    }
    let _serial = LAB_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    match prepare_cgroup(&cache) {
        Ok(own) => exercise(&own, &cache, &exe),
        Err(direct) if std::env::var_os("HEX_DELEGATED").is_none() => {
            rerun_delegated(TEST, &direct)
        }
        Err(direct) => {
            hex_testkit::skip(
                "oci_boxes_report_their_inits_exit_through_the_seam",
                "cgroup v2 delegation",
                &format!("none in the delegated scope either ({direct}); run on a cgroup-v2 host"),
            )
            .expect("recording the skip");
        }
    }
}

/// A delegated re-run of a test, to its end: no recorded operation makes it, and no census row times it yet, so the
/// wait is measuring, for the fact alone, each re-run's time recorded for the census to come, from the suites run under
/// it (hex_testkit::waits; the lead's ruling of 2026-11-08). It was the suite's recorded time and one program-end
/// patience more, a program's record taken for a whole suite.
static RERUN: Patience = Patience::measuring("test.oci_box.rerun");

// Each other wait here takes its own fact's record (hex_testkit::waits; the lead's rule of 2026-11-08), or measures
// where no recorded operation makes its fact, its answers recorded for the census to come.
/// A program's run, to its end or to what it does on its way (an init exec'ing its program, a program it runs writing
/// its mark, a program reaped once ended): the program-run record.
static PROGRAM_RUN: Patience = Patience::seeded("test.oci_box.program", PROGRAM);
/// A killed process's end, and its reaping: `node.emptying`.
static KILLED: Patience = Patience::seeded("test.oci_box.killed", EMPTYING);
/// What a create makes within itself, while the test holds it or as it answers (a create blocked reading its config or
/// writing its record, handed a seed, an init's stack mapped): the create's record. Each such test then asserts the
/// create still held, the fact made within it.
static CREATING: Patience = Patience::seeded("test.oci_box.create", CREATE);
/// An exec'd program running: made within the exec, before it answers.
static EXECUTING: Patience = Patience::seeded("test.oci_box.exec", EXEC);
/// A create's or delete's wait for the same box's call under way: `node.turns`.
static TURN: Patience = Patience::seeded("test.oci_box.turn", NODE_TURN);
/// Two namespaces made by the pool's refill thread, one after the other: two of `netns.make`'s.
static TWO_NAMESPACES: Patience =
    Patience::seeded("test.oci_box.two_namespaces", NETNS_MAKE.saturating_mul(2));
/// What the node's own threads make on their own, no recorded operation making it: the /dev pool filled, procs and
/// inits made ahead, procs dropped by the dropper, a dead seed's slot started again, an exec'd program's loader closing
/// its descriptors. Measuring, for the fact alone.
static DEVFS: Patience = Patience::measuring("test.oci_box.devfs");
static AHEAD: Patience = Patience::measuring("test.oci_box.ahead");
static DROPPED: Patience = Patience::measuring("test.oci_box.dropped");
static SEED_RESTARTED: Patience = Patience::measuring("test.oci_box.seed_restarted");
/// An orphan, let go, ending and reaped by the agent it was handed to: not a program's run, whose record the program-run
/// one is (waits-census-20261109T161828Z's suites ran this wait out at that record's 78.26 ms in all 79 loops), and made
/// by no recorded operation. Measuring, for the fact alone.
static ORPHAN_REAPED: Patience = Patience::measuring("test.oci_box.orphan_reaped");
static LOADER: Patience = Patience::measuring("test.oci_box.loader");

/// Re-runs the test `name` in a transient systemd --user scope with `Delegate=yes`, so it gets a cgroup of its
/// own to subdivide. Only where a user manager answers (its `$XDG_RUNTIME_DIR/systemd/private` socket is
/// there): a `systemd-run` binary with no manager running, as in a guest whose PID 1 isn't systemd, would wait
/// on a manager that never comes. Prints the reason and returns where there's none. The re-run is waited on
/// for at most [`RERUN`]'s deadline (none while it measures), then killed and failed by name.
fn rerun_delegated(name: &str, direct: &str) {
    let manager = std::env::var_os("XDG_RUNTIME_DIR")
        .map(|d| Path::new(&d).join("systemd/private"))
        .is_some_and(|p| p.exists());
    if !manager {
        hex_testkit::skip(
            name,
            "cgroup v2 delegation",
            &format!(
                "own cgroup isn't ours to subdivide ({direct}) and no systemd --user manager answers for a private scope"
            ),
        )
        .expect("recording the skip");
        return;
    }
    let test_bin = std::env::current_exe().expect("test binary path");
    let mut child = Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet", "-p", "Delegate=yes"])
        .arg(&test_bin)
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env("HEX_DELEGATED", "1")
        .spawn()
        .expect("run the test in a delegated scope");
    // The wait runs on a thread that sends the status when the child ends, so the test waits on that fact
    // for at most RERUN's deadline, and a hung re-run is killed by its pid.
    let pid = child.id();
    let (sent, ended) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sent.send(child.wait());
    });
    let began = Instant::now();
    let bound = RERUN.deadline().unwrap_or(Duration::MAX);
    let status = match ended.recv_timeout(bound) {
        Ok(status) => {
            hex_testkit::waits::answered(&RERUN, began.elapsed());
            status.expect("wait on the delegated re-run")
        }
        Err(_) => {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
            panic!(
                "the delegated re-run of {name} was still running after {bound:?}, so it was killed"
            );
        }
    };
    assert!(status.success(), "the delegated re-run failed");
}

/// AC-T7.1.4, goal 5 step 3: run the prepared-/dev ordering and isolation oracle independently of
/// the earlier full-suite checks. A failure in either test remains visible to the test harness.
#[test]
fn prepared_devs_match_ordinary_setup_and_isolate_containers() {
    if host_id("-u") != 0 {
        eprintln!("SKIP: the prepared /dev oracle requires the privileged root lab");
        return;
    }
    let _serial = LAB_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let parent = prepare_cgroup(&cache_dir()).expect("a delegated parent for the root oracle");
    let own = parent.join(format!("hex-devfs-oracle-{}", std::process::id()));
    std::fs::create_dir(&own).expect("a fresh cgroup for the independent oracle");
    // The full suite already moved our parent process into hex-agent. Move only this fresh child
    // into its own empty cgroup before exec, so its fixture can delegate controllers to its boxes.
    let output = Command::new("/bin/sh")
        .args([
            "-e",
            "-c",
            r#"printf '%s' "$$" > "$1/cgroup.procs"; shift; exec "$@""#,
            "hex-devfs-oracle",
        ])
        .arg(&own)
        .arg(std::env::current_exe().expect("test binary path"))
        .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(PREPARED_DEV_ENV, "1")
        .output()
        .expect("run the prepared /dev oracle through the root fixture");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    eprint!("{log}");
    assert!(
        output.status.success(),
        "the independent prepared /dev oracle failed"
    );
    assert!(
        log.lines().any(|line| line == PREPARED_DEV_PASS),
        "the root oracle did not run; a skipped delegation is not evidence"
    );
}

/// AC-T7.1.4 (runtime-tools' rootless subset, goal 17's rootless variant): a rootless `hex oci create` of a spec that
/// names no cgroupsPath makes the container's cgroup beside the command line's own, as crun's does. Do: run the case
/// alone through the lab's fixture, rootless. Expect it to pass, or, unless HEX_DELEGATED gave this binary a delegated
/// cgroup of its own, to say why it was skipped. As root it is no case: a root runtime writes the hierarchy's root.
#[test]
fn a_rootless_command_line_create_makes_its_cgroup_beside_its_own() {
    if host_id("-u") == 0 {
        eprintln!(
            "SKIP: the rootless command line's cgroup case runs rootless; as root /<id> is the root's to make"
        );
        return;
    }
    let _serial = LAB_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let exe = std::env::current_exe().expect("test binary path");
    // A child started here shares this process's cgroup (its own, or the hex-agent leaf the full suite moved it
    // into), which it can't subdivide while this process is in it. Where that cgroup is delegated to this binary,
    // start the child alone in a fresh cgroup beside the leaf, as the root oracles do; elsewhere the child finds its
    // own delegation (a systemd re-run) or says why there's none.
    let mut child = match prepare_cgroup(&cache_dir()) {
        Ok(parent) => {
            let own = parent.join(format!("hex-rootless-cli-{}", std::process::id()));
            std::fs::create_dir(&own).expect("a fresh cgroup for the rootless case");
            let mut placed = Command::new("/bin/sh");
            placed
                .args([
                    "-e",
                    "-c",
                    r#"printf '%s' "$$" > "$1/cgroup.procs"; shift; exec "$@""#,
                    "hex-rootless-cli",
                ])
                .arg(&own)
                .arg(&exe);
            placed
        }
        Err(_) => Command::new(&exe),
    };
    let output = child
        .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(ROOTLESS_CLI_CGROUP_ENV, "1")
        .output()
        .expect("run the rootless command line's cgroup case through the lab's fixture");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    eprint!("{log}");
    assert!(
        output.status.success(),
        "the rootless command line's cgroup case failed"
    );
    if !log.lines().any(|line| line == ROOTLESS_CLI_CGROUP_PASS) {
        // Under HEX_DELEGATED the runner gave this binary a delegated cgroup of its own, so the case runs there.
        assert!(
            std::env::var_os("HEX_DELEGATED").is_none(),
            "the rootless command line's cgroup case was skipped though this binary's cgroup is delegated to it"
        );
        // The fixture says why where it can't run a rootless container here (no delegation, no user namespaces).
        assert!(
            log.lines().any(|line| line.starts_with("SKIP")),
            "the rootless command line's cgroup case neither ran nor said why it was skipped"
        );
        eprintln!(
            "SKIP: the rootless command line's cgroup case was skipped by the lab's fixture, as it says above"
        );
    }
}

/// AC-T7.1.4: rootfs and bind sources are deliberately shared; every mount stays in the box.
#[test]
fn pruned_shared_sources_do_not_leak_mounts_to_the_agent() {
    if host_id("-u") != 0 {
        eprintln!("SKIP: the shared-source propagation oracle requires the privileged root lab");
        return;
    }
    let _serial = LAB_SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let parent = prepare_cgroup(&cache_dir()).expect("a delegated parent for the root oracle");
    let own = parent.join(format!("hex-propagation-oracle-{}", std::process::id()));
    std::fs::create_dir(&own).expect("a fresh cgroup for the independent oracle");
    // The full suite already moved our parent process into hex-agent. Move only this fresh child
    // into its own empty cgroup before exec, so its fixture can delegate controllers to its boxes.
    let output = Command::new("/bin/sh")
        .args([
            "-e",
            "-c",
            r#"printf '%s' "$$" > "$1/cgroup.procs"; shift; exec "$@""#,
            "hex-propagation-oracle",
        ])
        .arg(&own)
        .arg(std::env::current_exe().expect("test binary path"))
        .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(PROPAGATION_ENV, "1")
        .output()
        .expect("run the shared-source propagation oracle through the root fixture");
    let log = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    eprint!("{log}");
    assert!(
        output.status.success(),
        "the independent shared-source propagation oracle failed"
    );
    assert!(
        log.lines().any(|line| line == PROPAGATION_PASS),
        "the root oracle did not run; a skipped delegation is not evidence"
    );
}

/// Where one run keeps its bundles, records and box root.
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
            .prefix("ocibox-")
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

    /// A bundle whose init runs `args`, in a fresh directory.
    fn bundle(&self, args: &[String]) -> PathBuf {
        self.bundle_with(args, false)
    }

    /// A bundle whose init runs `args`, with a network namespace of its own when `network` is set,
    /// its sysfs mounted at `/sys` to read it by.
    fn bundle_with(&self, args: &[String], network: bool) -> PathBuf {
        let dir = tempfile::Builder::new()
            .prefix("bundle-")
            .tempdir_in(self.bundles.path())
            .expect("bundle dir")
            .keep();
        std::fs::write(dir.join("config.json"), self.config(args, network)).expect("config.json");
        dir
    }

    fn config(&self, args: &[String], network: bool) -> String {
        let (mut namespaces, mappings) = if self.rootless {
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
        let mut sys = String::new();
        if network {
            namespaces.push_str(r#",{"type":"network"}"#);
            sys.push_str(
                r#",
    {"destination": "/sys", "type": "sysfs", "source": "sysfs", "options": ["nosuid","noexec","nodev","ro"]}"#,
            );
        }
        self.config_with(args, &namespaces, &mappings, &sys)
    }

    /// A bundle's config.json with its `linux.namespaces` entries, id mappings and extra mounts as given.
    fn config_with(&self, args: &[String], namespaces: &str, mappings: &str, sys: &str) -> String {
        self.config_ex(args, namespaces, mappings, sys, "", "")
    }

    /// `config_with` with, besides, a top-level `hooks` fragment and extra fields in the `linux` object
    /// (`rootfsPropagation`), each a JSON fragment with its own leading comma or empty. Goal 5 plan step 2's
    /// classic-path reasons (createContainer hooks) and rootfsPropagation tests use it.
    fn config_ex(
        &self,
        args: &[String],
        namespaces: &str,
        mappings: &str,
        sys: &str,
        hooks: &str,
        linux_extra: &str,
    ) -> String {
        let args = json_args(args);
        format!(
            r#"{{
  "ociVersion": "1.3.0",
  "root": {{"path": "{rootfs}", "readonly": false}},
  "process": {{
    "cwd": "/",
    "args": {args},
    "env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
    "user": {{"uid": 0, "gid": 0}},
    "capabilities": {capabilities},
    "noNewPrivileges": true
  }},
  "mounts": [
    {{"destination": "/proc", "type": "proc", "source": "proc"}},
    {{"destination": "/dev", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid","mode=755","size=65536k"]}},
    {{"destination": "/tmp", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid","nodev"]}},
    {{"destination": "/usr", "type": "bind", "source": "/usr", "options": ["rbind","ro"]}},
    {{"destination": "/hex", "type": "bind", "source": "{exe}", "options": ["bind","ro"]}}{sys}
  ],
  "linux": {{"namespaces": [{namespaces}]{mappings}{linux_extra}}}{hooks}
}}"#,
            rootfs = self.rootfs.display(),
            exe = self.exe,
            capabilities = capabilities(),
        )
    }

    /// A box whose init runs `args`.
    fn running(&self, id: BoxId, args: &[String]) -> BoxSpec {
        BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id,
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(self.bundle(args)),
            owner: None,
            band: 0,
        }
    }

    /// A box whose init waits, in a network namespace of its own.
    fn networked(&self, id: BoxId) -> BoxSpec {
        BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id,
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(self.bundle_with(&Self::act(&Act::Wait), true)),
            owner: None,
            band: 0,
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

    fn node(&self, box_root: &Path, state: &Path) -> Node {
        Node::open(box_root, state, state)
            .expect("open a node")
            .with_oci_helper(PathBuf::from(&self.exe))
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
            class: ClassSpec::Bundle(self.bundle(&Self::act(&Act::Wait))),
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
            class: ClassSpec::Bundle(self.bundle(&Self::act(act))),
            owner: None,
            band: 0,
        }
    }

    fn program(&self, act: &Act) -> hex_api::Program {
        let args = Self::act(act);
        hex_api::Program::of(&args.iter().map(String::as_str).collect::<Vec<_>>())
    }
}

fn exercise(own: &Path, cache: &Path, exe: &str) {
    let lab = Lab::new(cache, exe);
    if std::env::var_os(PROPAGATION_ENV).is_some() {
        assert!(
            !lab.rootless,
            "the shared-source propagation oracle needs root"
        );
        rootfs_propagation_shows_in_the_box_not_the_agent(&lab, own);
        eprintln!("{PROPAGATION_PASS}");
        return;
    }
    if std::env::var_os(PREPARED_DEV_ENV).is_some() {
        assert!(!lab.rootless, "the prepared /dev oracle needs root");
        prepared_devs_preserve_order_and_are_never_shared(&lab, own);
        eprintln!("{PREPARED_DEV_PASS}");
        return;
    }
    if std::env::var_os(MASKED_DIRS_ENV).is_some() {
        assert!(!lab.rootless, "HEX-MNTNS-8's case needs root");
        masked_directories_are_the_nodes_one_empty_filesystem(&lab, own);
        return;
    }
    if std::env::var_os(ROOTLESS_CLI_CGROUP_ENV).is_some() {
        assert!(
            lab.rootless,
            "the rootless command line's cgroup case runs rootless"
        );
        rootless_cli_create_makes_its_cgroup_beside_its_own(&lab, own);
        eprintln!("{ROOTLESS_CLI_CGROUP_PASS}");
        return;
    }
    // First, while this process is no node's and so no subreaper; it also finds out whether a
    // container runs here at all (rootless needs unprivileged user namespaces).
    if let Err(e) = crash_and_restart(&lab, own) {
        hex_testkit::skip(
            "oci_boxes_report_their_inits_exit_through_the_seam",
            "unprivileged user namespaces",
            &format!("rootless OCI unavailable here ({e})"),
        )
        .expect("recording the skip");
        return;
    }
    parent_and_exit_code(&lab, own);
    killed_init(&lab, own);
    budget(&lab, own);
    conformance_on_oci(&lab, own);
    network_namespaces(&lab, own);
    program_alone(&lab, own);
    programs_hold_their_stdio_alone(&lab, own);
    exec_reads_no_state(&lab, own);
    orphans_are_reaped(&lab, own);
    held_record_write(&lab, own);
    two_nodes_one_box_id(&lab, own);
    records_in_both_places_are_found_after_a_restart(&lab, own);
    a_delete_racing_a_create_of_one_id_leaves_the_node_consistent(&lab, own);
    a_config_swapped_after_the_check_never_makes_the_container(&lab, own);
    an_exec_takes_the_checked_spec_not_the_bundle_swapped_after_create(&lab, own);
    a_start_runs_the_checked_specs_hooks_not_the_bundle_swapped_after_create(&lab, own);
    a_create_or_delete_of_a_box_being_made_waits_for_it(&lab, own);
    a_second_create_of_an_owned_box_being_made_waits_and_takes_no_slot(&lab, own);
    created_through_a_seed_in_its_cgroup_and_namespaces(&lab, own);
    each_seed_cloned_container_is_randomized_afresh(&lab, own);
    a_seed_that_dies_mid_create_is_refused_and_restarted(&lab, own);
    a_bundle_joining_a_namespace_by_path_runs_through_the_seam(&lab, own);
    a_box_root_counts_its_boxes_without_listing(&lab, own);
    escapes_are_contained(&lab, own);
    mount_namespaces_made_each_way(&lab, own);
    procs_made_ahead_carry_their_masks(&lab, own);
    a_parked_inits_proc_is_its_own_and_taken_once(&lab, own);
    a_parked_init_takes_a_create_made_each_way(&lab, own);
    masked_directories_are_the_nodes_one_empty_filesystem(&lab, own);
    create_seeds_share_the_nodes_one_pruned_namespace(&lab, own);
    a_bind_and_a_volume_are_present_the_host_unreachable(&lab, own);
    classic_reasons_are_counted(&lab, own);
    create_and_start_container_hooks_run_in_the_containers_namespaces(&lab, own);
    a_failing_create_container_hook_fails_the_create_naming_it(&lab, own);
    a_failing_start_container_hook_fails_the_start_naming_it(&lab, own);
    an_init_that_refuses_or_dies_mid_build_leaves_nothing(&lab, own);
    rootfs_propagation_shows_in_the_box_not_the_agent(&lab, own);
    every_mount_kind_is_in_the_box_as_its_spec_asks(&lab, own);
    prepared_devs_preserve_order_and_are_never_shared(&lab, own);
    a_program_past_the_kernels_bound_is_refused_by_name_and_learned(&lab, own);
}

/// What a box's `/dev` holds by name, as its spec and the runtime spec make it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum DevEntry {
    /// A character device of this major and minor, mode 0666, owned by root (runtime spec, "Default Devices").
    Char(u32, u32),
    /// A link to this target (runtime spec, "Dev symbolic links").
    Link(&'static str),
    /// A directory owned by root: a later mount's target, made where the name was missing, which the default
    /// device or link then leaves as it is.
    Dir,
}

/// What a box made from one of the oracle's cases must show, derived from its spec alone: the mounts in the spec's
/// order, the destination of each made as a directory where it is missing, and the default devices and links made
/// after them in whatever `/dev` resolves to, a name already there left as it is (runtime spec, "Default Devices"
/// and "Dev symbolic links"; runc's rootfs_linux.go makes them after the mounts too). `Err` holds what the
/// refusal must name.
struct DevExpected {
    /// Where `/dev`'s mount lands: `/dev`, or where a rootfs whose `/dev` is a link resolves it inside the root.
    dev_point: &'static str,
    /// The late tmpfs's mount point, where the case adds one.
    late_point: Option<&'static str>,
    /// Each name in `/dev`, in the spec's order of devices then links.
    entries: Vec<(&'static str, DevEntry)>,
}

/// The expectation for `case` with its late tmpfs at `target` (see [`DevExpected`]); `Err` with the words its
/// refusal must hold.
fn dev_expected(
    case: &str,
    target: Option<&'static str>,
) -> Result<DevExpected, Vec<&'static str>> {
    let shadowed = match case {
        // A destination with `..` is refused by the mount plan before anything is mounted.
        "parent" => return Err(vec!["/dev/shm/../null", "'..' component"]),
        // The link /alias leads to /dev/null, missing while the mounts are made: a link to nothing in the root.
        "alias" => return Err(vec!["/alias", "a link to nothing inside it"]),
        // /blocked is a file, so /blocked/child can't be resolved, let alone made.
        "file-component" => return Err(vec!["/blocked/child"]),
        // The late tmpfs takes /dev/null (its root a directory) or a directory made at /dev/null for its child, and
        // /dev/fd likewise, before the default device or link of that name is made.
        "default-node" | "through-node" => Some("null"),
        "default-link" => Some("fd"),
        _ => None,
    };
    let mut entries = vec![
        ("null", DevEntry::Char(1, 3)),
        ("zero", DevEntry::Char(1, 5)),
        ("full", DevEntry::Char(1, 7)),
        ("random", DevEntry::Char(1, 8)),
        ("urandom", DevEntry::Char(1, 9)),
        ("tty", DevEntry::Char(5, 0)),
        ("fd", DevEntry::Link("/proc/self/fd")),
        ("stdin", DevEntry::Link("/proc/self/fd/0")),
        ("stdout", DevEntry::Link("/proc/self/fd/1")),
        ("stderr", DevEntry::Link("/proc/self/fd/2")),
        ("ptmx", DevEntry::Link("pts/ptmx")),
    ];
    for (name, entry) in &mut entries {
        if Some(*name) == shadowed {
            *entry = DevEntry::Dir;
        }
    }
    Ok(DevExpected {
        dev_point: if case == "dev-symlink" {
            "/devices"
        } else {
            "/dev"
        },
        late_point: target,
        entries,
    })
}

/// AC-T7.1.4, goal 5 step 3: prepared /dev on hostile mount layouts, on the pruned path and on the classic one a
/// createRuntime hook takes through the shim (goal 5 extended step 3). Do: a box of each case on a root node, whose
/// /dev pool is always on (Ada's 2026-11-06 rule removed its off switch; ordinary setup is for a rootless node and a
/// dry pool). Expect what the spec alone derives ([`dev_expected`]): the refusals naming what they refuse; on success
/// each default device and link, or the directory a later mount took, and the box's mounts under `/dev` in the
/// spec's order with the spec's options; a pooled take for each case on each path, two actual pooled takes for live
/// containers, and no cross-container mutation or host-namespace attachment.
fn prepared_devs_preserve_order_and_are_never_shared(lab: &Lab, own: &Path) {
    if lab.rootless {
        return;
    }
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};
    let pooled_state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let pooled = lab.node(&own.join("devfs-pooled"), pooled_state.path());
    let before_mounts = agent_mount_points();

    let bundle = |case: &str, dest: Option<&str>, shim: bool| {
        let dir = tempfile::Builder::new()
            .prefix("devfs-bundle-")
            .tempdir_in(lab.bundles.path())
            .unwrap()
            .keep();
        let root = dir.join("rootfs");
        build_rootfs(&root);
        if case == "alias" {
            std::os::unix::fs::symlink("/dev/null", root.join("alias")).unwrap();
        }
        if case == "file-component" {
            std::fs::write(root.join("blocked"), "file").unwrap();
        }
        if case == "dev-symlink" {
            std::fs::rename(root.join("dev"), root.join("devices")).unwrap();
            std::os::unix::fs::symlink("devices", root.join("dev")).unwrap();
        }
        let mut spec: serde_json::Value =
            serde_json::from_str(&lab.config(&Lab::act(&Act::Wait), false)).unwrap();
        spec["root"]["path"] = serde_json::json!(root);
        if let Some(dest) = dest {
            spec["mounts"].as_array_mut().unwrap().push(serde_json::json!({
                "destination":dest, "type":"tmpfs", "source":"late", "options":["mode=701","size=64k"]
            }));
        }
        if shim {
            spec["hooks"] =
                serde_json::json!({"createRuntime":[{"path":"/usr/bin/true","args":["true"]}]});
        }
        std::fs::write(dir.join("config.json"), serde_json::to_vec(&spec).unwrap()).unwrap();
        dir
    };
    let create = |node: &Node, id: &BoxId, dir: &Path| {
        node.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(dir.to_owned()),
            owner: None,
            band: 0,
        })
    };
    let prime = bundle("prime", None, false);
    let id = BoxId::new("prime").unwrap();
    create(&pooled, &id, &prime).unwrap();
    pooled.delete(&id).unwrap();
    wait_for(
        &DEVFS,
        || pooled.dev_mounts().unwrap().made >= pooled.sizes().start as u64,
        "the prepared /dev pool to fill after its first key",
    );
    assert_eq!(pooled.dev_mounts().unwrap().failed, 0);
    let root_of = |node: &Node, id: &BoxId| {
        PathBuf::from(format!("/proc/{}/root", init_pid(&node.container_dir(id))))
    };
    // The box's `/dev` entries by name and its mounts at or under where `/dev` landed, or the late tmpfs's, in
    // mountinfo's order (the order they were made), each checked against `want`.
    let check = |node: &Node, id: &BoxId, want: &DevExpected, what: &str| {
        let root = root_of(node, id);
        for (name, entry) in &want.entries {
            let path = root.join("dev").join(name);
            let m = std::fs::symlink_metadata(&path)
                .unwrap_or_else(|e| panic!("{what}: /dev/{name}: {e}"));
            assert_eq!((m.uid(), m.gid()), (0, 0), "{what}: /dev/{name}'s owner");
            match entry {
                DevEntry::Char(major, minor) => {
                    assert!(
                        m.file_type().is_char_device(),
                        "{what}: /dev/{name} is {m:?}"
                    );
                    assert_eq!(m.mode() & 0o7777, 0o666, "{what}: /dev/{name}'s mode");
                    assert_eq!(
                        m.rdev(),
                        rustix::fs::makedev(*major, *minor),
                        "{what}: /dev/{name} is {major}:{minor}"
                    );
                }
                DevEntry::Link(target) => {
                    assert!(m.file_type().is_symlink(), "{what}: /dev/{name} is {m:?}");
                    assert_eq!(
                        std::fs::read_link(&path).unwrap(),
                        PathBuf::from(target),
                        "{what}: /dev/{name}'s target"
                    );
                }
                DevEntry::Dir => assert!(m.is_dir(), "{what}: /dev/{name} is {m:?}"),
            }
        }
        let pid = init_pid(&node.container_dir(id));
        let under = format!("{}/", want.dev_point);
        let got: Vec<ListedMount> =
            listed_mounts(&std::fs::read_to_string(format!("/proc/{pid}/mountinfo")).unwrap())
                .into_iter()
                .filter(|m| {
                    m.point == want.dev_point || m.point.starts_with(&under) || m.source == "late"
                })
                .collect();
        let points: Vec<(&str, &str, &str)> = got
            .iter()
            .map(|m| (m.point.as_str(), m.fstype.as_str(), m.source.as_str()))
            .collect();
        let mut wanted = vec![(want.dev_point, "tmpfs", "tmpfs")];
        wanted.extend(want.late_point.map(|p| (p, "tmpfs", "late")));
        assert_eq!(
            points, wanted,
            "{what}: the mounts under /dev, in the spec's order"
        );
        let has = |m: &ListedMount, opts: &[&str], sb: &[&str]| {
            opts.iter().all(|o| m.opts.iter().any(|x| x == o))
                && sb.iter().all(|o| m.sb_opts.iter().any(|x| x == o))
        };
        assert!(
            has(&got[0], &["nosuid"], &["mode=755", "size=65536k"]),
            "{what}: /dev as its spec mounts it: {:?}",
            got[0]
        );
        if let Some(late) = got.get(1) {
            assert!(
                has(late, &[], &["mode=701", "size=64k"]),
                "{what}: the late tmpfs as its spec mounts it: {late:?}"
            );
        }
    };
    for (case, target, shim) in [
        ("plain", None),
        ("dev-symlink", None),
        ("default-node", Some("/dev/null")),
        ("through-node", Some("/dev/null/child")),
        ("default-link", Some("/dev/fd/extra")),
        ("alias", Some("/alias")),
        ("file-component", Some("/blocked/child")),
        ("parent", Some("/dev/shm/../null")),
    ]
    .into_iter()
    .flat_map(|(case, target)| [(case, target, false), (case, target, true)])
    {
        let dir = bundle(case, target, shim);
        let id = BoxId::new(&format!("{case}-{}", if shim { "shim" } else { "pruned" })).unwrap();
        let what = format!("{case} (through the shim: {shim})");
        let through_shim = pooled.creates_through_shim();
        if case != "parent" {
            wait_for(
                &DEVFS,
                || {
                    let c = pooled.dev_mounts().unwrap();
                    c.made > c.taken + c.discarded
                },
                "a prepared mount for the oracle case",
            );
        }
        let taken = pooled.dev_mounts().unwrap().taken;
        match (dev_expected(case, target), create(&pooled, &id, &dir)) {
            (Ok(want), Ok(_)) => {
                check(&pooled, &id, &want, &what);
                pooled.delete(&id).unwrap();
            }
            (Err(words), Err(e)) => {
                let said = e.to_string();
                for w in words {
                    assert!(said.contains(w), "{what}: the refusal names {w:?}: {said}");
                }
            }
            (Ok(_), Err(e)) => panic!("{what}: refused where its spec makes a box: {e}"),
            (Err(_), Ok(_)) => {
                pooled.delete(&id).unwrap();
                panic!("{what}: made where its spec is refused");
            }
        }
        if case != "parent" {
            assert_eq!(
                pooled.dev_mounts().unwrap().taken,
                taken + 1,
                "{what} must consume a prepared mount (including guarded fallback)"
            );
            assert_eq!(
                pooled.creates_through_shim() - through_shim,
                u64::from(shim),
                "{what} took the path it was meant to"
            );
        }
    }

    let first = BoxId::new("isolated-a").unwrap();
    let second = BoxId::new("isolated-b").unwrap();
    wait_for(
        &DEVFS,
        || {
            let c = pooled.dev_mounts().unwrap();
            c.made >= c.taken + c.discarded + 2
        },
        "two prepared mounts for isolation",
    );
    let taken = pooled.dev_mounts().unwrap().taken;
    create(&pooled, &first, &prime).unwrap();
    create(&pooled, &second, &prime).unwrap();
    assert_eq!(pooled.dev_mounts().unwrap().taken, taken + 2);
    let aroot = root_of(&pooled, &first);
    let broot = root_of(&pooled, &second);
    assert_ne!(
        std::fs::metadata(aroot.join("dev")).unwrap().dev(),
        std::fs::metadata(broot.join("dev")).unwrap().dev(),
        "a fresh superblock per container"
    );
    std::fs::write(aroot.join("dev/private-marker"), "first only").unwrap();
    std::fs::set_permissions(
        aroot.join("dev/null"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    assert!(!broot.join("dev/private-marker").exists());
    assert_eq!(
        std::fs::metadata(broot.join("dev/null")).unwrap().mode() & 0o777,
        0o666
    );
    pooled.delete(&first).unwrap();
    assert!(
        std::fs::metadata(broot.join("dev/zero"))
            .unwrap()
            .file_type()
            .is_char_device()
    );
    pooled.delete(&second).unwrap();
    assert_eq!(
        agent_mount_points(),
        before_mounts,
        "prepared mounts never attach in the agent"
    );
    eprintln!(
        "prepared /dev: fixed pool counters, the spec-derived ordering/refusal oracle, devices/links/options, distinct superblocks and isolated mutation all pass"
    );
}

/// Goal 5 plan step 3 (T-T7.1.4; CLAUDE.md §6): every mount kind a spec can ask for is in the box's own
/// mountinfo with the source, target and options its spec asked, on each root path. One spec asks for
/// them all: proc, tmpfs (`/dev`, `/tmp`, `/dev/shm`, and two whose access times the spec sets:
/// `strictatime`, as Docker's `/dev` asks, and `noatime,nodiratime`), devpts, mqueue, sysfs and a
/// read-only cgroup2 in the box's own cgroup namespace, a recursive bind read-only (`/usr`) and
/// read-write (a volume), a plain bind of a directory and of a file read-only (the file's target missing
/// from the image, so it is made), masked paths (files and a directory) and read-only paths (a directory
/// and a file). As root the pruned path makes one box and the classic path
/// another, a createRuntime hook routing it through the shim; rootless the classic path makes it, in the
/// box's own user namespace. Each box is checked the same way, so a change to how the init mounts that
/// alters what a mount is fails here on the path it changed.
fn every_mount_kind_is_in_the_box_as_its_spec_asks(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-mount-kinds"), state.path());
    let vol = lab.bundles.path().join("kinds-vol");
    let ro_dir = lab.bundles.path().join("kinds-ro");
    let file = lab.bundles.path().join("kinds-file");
    for dir in [&vol, &ro_dir] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("marker"), "from the host\n").unwrap();
    }
    std::fs::write(&file, "a bound file\n").unwrap();
    // A rootless box's user namespace has no group 5 to give its terminals (bundle.rs, default_mounts).
    let pts = if lab.rootless {
        r#"["nosuid","noexec","newinstance","ptmxmode=0666","mode=0620"]"#
    } else {
        r#"["nosuid","noexec","newinstance","ptmxmode=0666","mode=0620","gid=5"]"#
    };
    let mounts = format!(
        r#",
    {{"destination": "/dev/pts", "type": "devpts", "source": "devpts", "options": {pts}}},
    {{"destination": "/dev/shm", "type": "tmpfs", "source": "shm", "options": ["nosuid","noexec","nodev","mode=1777","size=65536k"]}},
    {{"destination": "/dev/mqueue", "type": "mqueue", "source": "mqueue", "options": ["nosuid","noexec","nodev"]}},
    {{"destination": "/sys", "type": "sysfs", "source": "sysfs", "options": ["nosuid","noexec","nodev","ro"]}},
    {{"destination": "/sys/fs/cgroup", "type": "cgroup2", "source": "cgroup", "options": ["nosuid","noexec","nodev","ro"]}},
    {{"destination": "/strict", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid","strictatime","mode=755"]}},
    {{"destination": "/noatime", "type": "tmpfs", "source": "tmpfs", "options": ["noatime","nodiratime"]}},
    {{"destination": "/vol", "type": "bind", "source": "{vol}", "options": ["rbind","rw"]}},
    {{"destination": "/ro-dir", "type": "bind", "source": "{ro_dir}", "options": ["bind","ro"]}},
    {{"destination": "/etc/kinds-file", "type": "bind", "source": "{file}", "options": ["bind","ro"]}}"#,
        vol = vol.display(),
        ro_dir = ro_dir.display(),
        file = file.display(),
    );
    // Masks the kernel may lack are looked for in the agent's own /proc and /sys (one kernel), and the box
    // is held to a mask only where the path is there to mask.
    let masked = [
        "/proc/kcore",
        "/proc/timer_list",
        "/proc/scsi",
        "/sys/firmware",
    ];
    let readonly = ["/proc/sys", "/proc/sysrq-trigger"];
    let linux_extra = format!(
        r#","maskedPaths":{},"readonlyPaths":{}"#,
        json_args(&masked.map(str::to_owned)),
        json_args(&readonly.map(str::to_owned))
    );
    let (namespaces, mappings) = if lab.rootless {
        (
            r#"{"type":"user"},{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"},{"type":"network"},{"type":"cgroup"}"#
                .to_owned(),
            format!(
                r#","uidMappings":[{{"containerID":0,"hostID":{},"size":1}}],"gidMappings":[{{"containerID":0,"hostID":{},"size":1}}]"#,
                lab.uid, lab.gid
            ),
        )
    } else {
        (
            r#"{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"},{"type":"network"},{"type":"cgroup"}"#
                .to_owned(),
            String::new(),
        )
    };
    let hooks = r#","hooks":{"createRuntime":[{"path":"/usr/bin/true","args":["true"]}]}"#;
    // (the path, its hooks): rootless the classic path alone; as root the pruned path, and the classic one,
    // which a create-time hook takes through the shim.
    let paths: &[(&str, &str)] = if lab.rootless {
        &[("classic", "")]
    } else {
        &[("pruned", ""), ("classic", hooks)]
    };
    for &(path, hooks) in paths {
        let dir = tempfile::Builder::new()
            .prefix("bundle-")
            .tempdir_in(lab.bundles.path())
            .unwrap()
            .keep();
        std::fs::write(
            dir.join("config.json"),
            lab.config_ex(
                &Lab::act(&Act::Wait),
                &namespaces,
                &mappings,
                &mounts,
                hooks,
                &linux_extra,
            ),
        )
        .unwrap();
        let id = BoxId::new(&format!("kinds-{path}")).unwrap();
        let (pruned, shim) = (node.pruned_creates(), node.creates_through_shim());
        node.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(dir),
            owner: None,
            band: 0,
        })
        .unwrap_or_else(|e| panic!("create the {path} box: {e:?}"));
        node.start(&id).expect("start");
        let took = (
            node.pruned_creates() - pruned,
            node.creates_through_shim() - shim,
        );
        let (exit, mi) = run_in(&node, &id, &["/usr/bin/cat", "/proc/self/mountinfo"]);
        let (vol_exit, vol_out) = run_in(&node, &id, &["/usr/bin/cat", "/vol/marker"]);
        let ro_write = exit_in(&node, &id, &["/usr/bin/touch", "/ro-dir/written"]);
        node.delete(&id).expect("delete");

        let want_took = match (lab.rootless, path) {
            (true, _) => (0, 0),
            (false, "pruned") => (1, 0),
            _ => (0, 1),
        };
        assert_eq!(
            took, want_took,
            "the {path} box was made on the {path} path (pruned, through the shim)"
        );
        assert_eq!(exit, Exit::Code(0), "read the {path} box's mountinfo: {mi}");
        let listed = listed_mounts(&mi);
        let at = |point: &str| {
            listed
                .iter()
                .rev()
                .find(|m| m.point == point)
                .unwrap_or_else(|| panic!("no mount at {point} in the {path} box:\n{mi}"))
        };
        let has = |m: &ListedMount, opts: &[&str], sb: &[&str]| {
            for o in opts {
                assert!(
                    m.opts.iter().any(|x| x == o),
                    "{} in the {path} box has {o}: {m:?}",
                    m.point
                );
            }
            for o in sb {
                assert!(
                    m.sb_opts.iter().any(|x| x == o),
                    "{}'s filesystem in the {path} box has {o}: {m:?}",
                    m.point
                );
            }
        };
        // Filesystems: the type and source the spec named, its per-mount and filesystem options.
        let proc = at("/proc");
        assert_eq!(
            (proc.fstype.as_str(), proc.source.as_str()),
            ("proc", "proc")
        );
        has(proc, &["rw"], &[]);
        let dev = at("/dev");
        assert_eq!(
            (dev.fstype.as_str(), dev.source.as_str()),
            ("tmpfs", "tmpfs")
        );
        has(dev, &["rw", "nosuid"], &["mode=755", "size=65536k"]);
        let tmp = at("/tmp");
        assert_eq!(
            (tmp.fstype.as_str(), tmp.source.as_str()),
            ("tmpfs", "tmpfs")
        );
        has(tmp, &["rw", "nosuid", "nodev"], &[]);
        let pts = at("/dev/pts");
        assert_eq!(
            (pts.fstype.as_str(), pts.source.as_str()),
            ("devpts", "devpts")
        );
        let pts_sb: &[&str] = if lab.rootless {
            &["mode=620", "ptmxmode=666"]
        } else {
            &["mode=620", "ptmxmode=666", "gid=5"]
        };
        has(pts, &["rw", "nosuid", "noexec"], pts_sb);
        let shm = at("/dev/shm");
        assert_eq!((shm.fstype.as_str(), shm.source.as_str()), ("tmpfs", "shm"));
        has(shm, &["rw", "nosuid", "nodev", "noexec"], &["size=65536k"]);
        // Its mode=1777 is tmpfs's own default, which tmpfs lists by naming no mode (v6.19
        // mm/shmem.c:4930-4931), where /dev's mode=755 is named.
        assert!(
            !shm.sb_opts.iter().any(|o| o.starts_with("mode=")),
            "/dev/shm in the {path} box has tmpfs's default mode, 1777, as asked: {shm:?}"
        );
        let mq = at("/dev/mqueue");
        assert_eq!(
            (mq.fstype.as_str(), mq.source.as_str()),
            ("mqueue", "mqueue")
        );
        has(mq, &["rw", "nosuid", "nodev", "noexec"], &[]);
        let sys = at("/sys");
        assert_eq!(
            (sys.fstype.as_str(), sys.source.as_str()),
            ("sysfs", "sysfs")
        );
        has(sys, &["ro", "nosuid", "nodev", "noexec"], &[]);
        let cgroup = at("/sys/fs/cgroup");
        assert_eq!(
            (cgroup.fstype.as_str(), cgroup.source.as_str()),
            ("cgroup2", "cgroup")
        );
        has(cgroup, &["ro", "nosuid", "nodev", "noexec"], &[]);
        // Access times: strictatime shows as neither relatime nor noatime (proc_pid_mountinfo(5)).
        let strict = at("/strict");
        assert_eq!(
            (strict.fstype.as_str(), strict.source.as_str()),
            ("tmpfs", "tmpfs")
        );
        has(strict, &["rw", "nosuid"], &["mode=755"]);
        assert!(
            !strict
                .opts
                .iter()
                .any(|o| o == "relatime" || o == "noatime"),
            "/strict in the {path} box keeps every access time, as strictatime asks: {strict:?}"
        );
        let noatime = at("/noatime");
        assert_eq!(
            noatime.fstype, "tmpfs",
            "/noatime in the {path} box: {noatime:?}"
        );
        has(noatime, &["rw", "noatime", "nodiratime"], &[]);
        // Binds: the source's filesystem and the source within it, read-only or read-write as asked.
        for (point, source, opt) in [
            ("/usr", Path::new("/usr"), "ro"),
            ("/hex", Path::new(&lab.exe), "ro"),
            ("/vol", vol.as_path(), "rw"),
            ("/ro-dir", ro_dir.as_path(), "ro"),
            ("/etc/kinds-file", file.as_path(), "ro"),
        ] {
            let m = at(point);
            assert_eq!(
                (m.dev.clone(), m.root.clone()),
                bind_source_of(source),
                "{point} in the {path} box is a bind of {}: {m:?}",
                source.display()
            );
            has(m, &[opt], &[]);
        }
        assert_eq!(
            (vol_exit, vol_out.trim()),
            (Exit::Code(0), "from the host"),
            "the {path} box reads its volume"
        );
        assert_ne!(
            ro_write,
            Exit::Code(0),
            "the {path} box can't write its read-only bind"
        );
        // Masks: a file under a bind of a /dev/null, a directory under an empty read-only tmpfs.
        for point in masked {
            let m = listed.iter().rev().find(|m| m.point == point);
            match (Path::new(point).exists(), Path::new(point).is_dir(), m) {
                (false, _, _) => {}
                (true, true, Some(m)) => {
                    assert_eq!(m.fstype, "tmpfs", "{point} masked in the {path} box: {m:?}");
                    has(m, &["ro"], &[]);
                }
                (true, false, Some(m)) => assert!(
                    m.root.ends_with("/null"),
                    "{point} in the {path} box is masked by a /dev/null: {m:?}"
                ),
                (true, _, None) => panic!("{point} isn't masked in the {path} box:\n{mi}"),
            }
        }
        // Read-only paths: each bound over itself, read-only, still the box's proc.
        for point in readonly {
            let m = at(point);
            assert_eq!(m.fstype, "proc", "{point} in the {path} box: {m:?}");
            assert_eq!(
                m.root,
                point.trim_start_matches("/proc"),
                "{point} in the {path} box is its own proc entry: {m:?}"
            );
            has(m, &["ro"], &[]);
        }
    }
    eprintln!(
        "mount kinds: proc, tmpfs, devpts, mqueue, sysfs, binds (recursive, plain, a file, read-only and \
         read-write), masks and read-only paths are each as their spec asks, on the {} path(s)",
        paths
            .iter()
            .map(|(p, _)| *p)
            .collect::<Vec<_>>()
            .join(" and ")
    );
}

/// What a bind of `path` lists in mountinfo as its filesystem and its root within it: the device of the
/// agent's mount that holds `path` (the topmost at the longest mount point it lies under), and that mount's
/// root joined to `path` below its mount point. A filesystem's own device, as mountinfo names it, can differ
/// from a file's `st_dev` (a btrfs subvolume's is its own), so the bind is matched by the table's.
fn bind_source_of(path: &Path) -> (String, String) {
    let real = std::fs::canonicalize(path).unwrap();
    let mounts = listed_mounts(&std::fs::read_to_string("/proc/self/mountinfo").unwrap());
    let holder = mounts
        .iter()
        .filter(|m| real.starts_with(&m.point))
        .max_by_key(|m| Path::new(&m.point).components().count())
        .map(|deepest| {
            mounts
                .iter()
                .rev()
                .find(|m| m.point == deepest.point)
                .unwrap()
        })
        .unwrap_or_else(|| panic!("no mount holds {}", real.display()));
    let below = real.strip_prefix(&holder.point).unwrap();
    // `path` may be the mount point itself (a lab that binds the host's /usr in), and joining an
    // empty path adds a trailing separator mountinfo never shows.
    let root = if below.as_os_str().is_empty() {
        PathBuf::from(&holder.root)
    } else {
        Path::new(&holder.root).join(below)
    };
    (holder.dev.clone(), root.to_string_lossy().into_owned())
}

/// One `/proc/<pid>/mountinfo` line (proc_pid_mountinfo(5)): the filesystem's device, the mount's root
/// within it, the mount point, the per-mount options, the filesystem type, its source and the
/// filesystem's own options.
#[derive(Debug)]
struct ListedMount {
    dev: String,
    root: String,
    point: String,
    opts: Vec<String>,
    fstype: String,
    source: String,
    sb_opts: Vec<String>,
}

fn listed_mounts(text: &str) -> Vec<ListedMount> {
    let split = |s: &str| s.split(',').map(str::to_owned).collect();
    text.lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let left: Vec<&str> = left.split(' ').collect();
            let right: Vec<&str> = right.split(' ').collect();
            Some(ListedMount {
                dev: (*left.get(2)?).to_owned(),
                root: (*left.get(3)?).to_owned(),
                point: (*left.get(4)?).to_owned(),
                opts: split(left.get(5)?),
                fstype: (*right.first()?).to_owned(),
                source: (*right.get(1)?).to_owned(),
                sb_opts: split(right.get(2)?),
            })
        })
        .collect()
}

/// Goal 5 plan step 2 (T-T7.1.4), the pruned-seed root's security criteria: a container on that path
/// can't reach the host (the research probe's E0-E3, `goal5-root.md` §3-§4; f66-f80). As root — where a
/// create takes the pruned seed — run each escape in its own box with full capabilities and a marker on
/// the host, outside any rootfs; each must exit 0, the box held. E0 a nested user namespace is allowed
/// (the root isn't a plain chroot); E1 the classic chroot escape stays in the rootfs; E2 unmounting the
/// root and `setns`-ing into the box's own mount namespace reveals only the seed's empty read-only tmpfs,
/// not the host; E3 a nested proc mount past a mask is refused. Skipped rootless, where the classic path
/// runs (a pruned seed is a root node's).
fn escapes_are_contained(lab: &Lab, own: &Path) {
    if lab.rootless {
        return;
    }
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-escape"), state.path());
    escapes_held_on(lab, &node, "esc");
    // The pruned path served every escape box: none fell back to the shim (all eligible).
    assert_eq!(
        node.creates_through_shim(),
        0,
        "the escape boxes take the pruned path"
    );
    eprintln!(
        "escapes contained: e0 userns allowed, e1/e2 marker unreached, e3 nested proc refused"
    );
}

/// Runs each escape box (`hex selftest-box escape`, e0-e3) on `node`, ids prefixed `tag`, and holds each
/// to exiting 0: e0 a nested user namespace allowed, e1 and e2 a root-only marker beside the rootfs
/// unreached, e3 a nested proc past a mask refused.
fn escapes_held_on(lab: &Lab, node: &Node, tag: &str) {
    // A marker only root can read, a sibling of the rootfs and so outside any box's root: a box that
    // opens it has reached the host.
    let marker = lab.bundles.path().join("escape-marker");
    std::fs::write(&marker, "hex-escape-marker").unwrap();
    let marker = marker.to_string_lossy().into_owned();
    for mode in ["e0", "e1", "e2", "e3"] {
        let id = BoxId::new(&format!("{tag}-{mode}")).unwrap();
        let spec = lab.running(
            id.clone(),
            &[
                "/hex".to_owned(),
                "selftest-box".to_owned(),
                "escape".to_owned(),
                mode.to_owned(),
                marker.clone(),
            ],
        );
        // Full capabilities, as the escapes need (E3 masks /proc/kcore with a mount): the lab's bundles
        // name Docker's default sets (b1c8e99), so this one names none and keeps its creator's.
        let ClassSpec::Bundle(bundle) = &spec.class else {
            panic!("lab.running makes a bundle")
        };
        let config = std::fs::read_to_string(bundle.join("config.json")).unwrap();
        let config: String = config
            .lines()
            .filter(|l| !l.trim_start().starts_with(r#""capabilities":"#))
            .map(|l| format!("{l}\n"))
            .collect();
        std::fs::write(bundle.join("config.json"), config).unwrap();
        node.create(spec).expect("create the escape box");
        node.start(&id).expect("start the escape box");
        let info = wait_exit(node, &id);
        assert_eq!(
            info.exit,
            Some(Exit::Code(0)),
            "escape {mode} is contained on {tag} (the box held)"
        );
        node.delete(&id).expect("delete the escape box");
    }
}

/// HEX-MNTNS-2, HEX-MNTNS-3, HEX-MNTNS-4 (decision 6's update, T-T7.1.4): ask a root node for each way of
/// making a pruned create's mount namespace (copy, empty, from its tree, and none), make a box with a
/// read-only `/usr` bind and run it, then the escape boxes. Expect each box counted under the way its seed
/// made the namespace: from its tree where the probe finds the call (Linux 7.0) and the asked node refuses
/// the create by type where it doesn't; empty on Linux 7.1 and the seed's copy below it (the seed's
/// fallback, counted as made); the default the probe's. Expect `/usr` read-only and the root writable in
/// each box, every escape held, no create through the shim, and the agent's own mount table the same
/// after as before.
fn mount_namespaces_made_each_way(lab: &Lab, own: &Path) {
    use hex_runtime::linux::oci::mntns::{self, MountNamespace, OpenTreeNamespace};
    if lab.rootless {
        return;
    }
    let probe = mntns::probe().expect("probe open_tree(OPEN_TREE_NAMESPACE)");
    let empty_here = kernel_at_least(7, 1);
    eprintln!("probe {probe:?}");
    let agent_before = agent_mount_points();
    for (asked, want) in [
        ("copy", Some(MountNamespace::Copy)),
        (
            "empty",
            Some(if empty_here {
                MountNamespace::Empty
            } else {
                MountNamespace::Copy
            }),
        ),
        (
            "tree",
            if probe == OpenTreeNamespace::Available {
                Some(MountNamespace::FromTree)
            } else {
                None
            },
        ),
        ("default", Some(mntns::default_way(probe).unwrap())),
    ] {
        let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
        let node = Node::open(
            &own.join(format!("hex-oci-mntns-{asked}")),
            state.path(),
            state.path(),
        )
        .expect("open a node");
        let node = if asked == "default" {
            node
        } else {
            node.with_mount_namespace(asked)
        }
        .with_oci_helper(PathBuf::from(&lab.exe));
        let id = BoxId::new(&format!("mntns-{asked}")).unwrap();
        let dir = tempfile::Builder::new()
            .prefix("bundle-")
            .tempdir_in(lab.bundles.path())
            .unwrap()
            .keep();
        std::fs::write(
            dir.join("config.json"),
            lab.config_with(
                &Lab::act(&Act::Wait),
                r#"{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"}"#,
                "",
                "",
            ),
        )
        .unwrap();
        let made = node.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(dir),
            owner: None,
            band: 0,
        });
        let Some(want) = want else {
            assert!(
                made.is_err(),
                "a namespace from its tree asked where it can't be made is refused ({probe:?})"
            );
            continue;
        };
        made.unwrap_or_else(|e| panic!("create a box with its namespace made {asked}: {e}"));
        node.start(&id).expect("start");
        let (mi_exit, mi) = run_in(&node, &id, &["/usr/bin/cat", "/proc/self/mountinfo"]);
        let root_write = exit_in(&node, &id, &["/usr/bin/touch", "/written"]);
        node.delete(&id).expect("delete");
        assert_eq!(
            node.mount_namespaces_made(want),
            1,
            "asked {asked}, the box's namespace was made {want:?}"
        );
        assert_eq!(node.creates_through_shim(), 0, "asked {asked}: no shim");
        assert_eq!(mi_exit, Exit::Code(0), "read the box's mountinfo: {mi}");
        let mounts = mountinfo(&mi);
        let usr = mounts
            .iter()
            .rev()
            .find(|(point, ..)| point == "/usr")
            .unwrap_or_else(|| panic!("/usr in the box made {asked}: {mi}"));
        assert!(
            usr.1.split(',').any(|o| o == "ro"),
            "/usr read-only: {usr:?}"
        );
        assert_eq!(
            root_write,
            Exit::Code(0),
            "asked {asked}: the root is writable"
        );
        escapes_held_on(lab, &node, &format!("esc-{asked}"));
        assert_eq!(
            node.creates_through_shim(),
            0,
            "asked {asked}: escapes made pruned"
        );
        assert_eq!(
            node.mount_namespaces_made(want),
            5,
            "asked {asked}: the escape boxes' namespaces were made {want:?} too"
        );
        eprintln!("mount namespace asked {asked}: made {want:?}, escapes held");
    }
    assert_eq!(
        agent_mount_points(),
        agent_before,
        "the agent's mount table is unchanged by every way"
    );
}

/// AC-T7.1.4 (runtime-tools' rootless subset; goal 17's rootless variant): through the command line, rootless, make a
/// container whose spec names no cgroupsPath, then delete it. Expect the create to succeed with the container's cgroup
/// beside the command line's own (the agent's leaf's sibling under the cgroup delegated to this user, `<own>/<id>`),
/// holding its init, as crun 1.30.1's cgroupfs driver makes one where the hierarchy's root can't be written
/// (`make_new_sibling_cgroup`, src/libcrun/cgroup-cgroupfs.c:96-110, taken at :128-155); and gone after the delete.
/// The hierarchy's root is root's alone, so `/<id>` there is refused to every rootless runtime (EACCES).
fn rootless_cli_create_makes_its_cgroup_beside_its_own(lab: &Lab, own: &Path) {
    let root = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let id = "cli-rootless-cg";
    let bundle = lab.bundle(&Lab::act(&Act::Wait));
    let log = root.path().join("create.log");
    // The command line's create passes its streams on to the container's init, as runc's does: the null device and a
    // file here, so no pipe the waiting init holds keeps this test waiting.
    let created = Command::new(&lab.exe)
        .args(["oci", "create", "--bundle", bundle.to_str().unwrap(), id])
        .env("HEX_OCI_ROOT", root.path())
        .stdin(Std::null())
        .stdout(Std::null())
        .stderr(std::fs::File::create(&log).unwrap())
        .status()
        .unwrap();
    let made = own.join(id);
    let procs = std::fs::read_to_string(made.join("cgroup.procs"));
    let deleted = Command::new(&lab.exe)
        .args(["oci", "delete", "--force", id])
        .env("HEX_OCI_ROOT", root.path())
        .output()
        .unwrap();
    assert!(
        created.success(),
        "a rootless create of a spec naming no cgroupsPath: {}",
        std::fs::read_to_string(&log).unwrap_or_default()
    );
    assert!(
        procs.as_ref().is_ok_and(|p| !p.trim().is_empty()),
        "the container's cgroup beside the command line's own, {}, holds its init: {procs:?}",
        made.display()
    );
    assert!(
        deleted.status.success(),
        "hex oci delete: {}",
        String::from_utf8_lossy(&deleted.stderr)
    );
    assert!(
        !made.exists(),
        "the delete removed the container's cgroup {}",
        made.display()
    );
}

/// HEX-MNTNS-6 (decision 6's update, part 2; T-T7.1.4): on a root node, make two boxes whose spec masks and
/// makes read-only paths beneath `/proc`, the second once the node has made a proc ahead. Expect the second's
/// proc taken from the pool (counted), its init pid 1 of the pid namespace that proc shows, and each box to
/// see the same: every mask and read-only path at the same point with the same read-only flag, a masked
/// file empty, a masked directory empty, a read-only path refusing a write, a missing path skipped.
fn procs_made_ahead_carry_their_masks(lab: &Lab, own: &Path) {
    if lab.rootless {
        return;
    }
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    // No inits parked ahead (decision 8): a parked init has its pid namespace already, so a create that claims one
    // asks the proc pool nothing, and this case is the pool's own.
    let node = lab
        .node(&own.join("hex-oci-procs"), state.path())
        .with_inits_ahead(0);
    let paths = r#","maskedPaths":["/proc/kcore","/proc/keys","/proc/timer_list","/proc/acpi","/proc/scsi","/proc/not-there"],"readonlyPaths":["/proc/sys","/proc/sysrq-trigger","/proc/bus"]"#;
    let dir = tempfile::Builder::new()
        .prefix("bundle-")
        .tempdir_in(lab.bundles.path())
        .unwrap()
        .keep();
    std::fs::write(
        dir.join("config.json"),
        lab.config_ex(
            &Lab::act(&Act::Wait),
            r#"{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"}"#,
            "",
            "",
            "",
            paths,
        ),
    )
    .unwrap();
    // What a box sees of its proc: each mount beneath it with whether it is read-only, a masked file's
    // size, whether a masked directory lists anything, whether a read-only path takes a write, and its
    // pid 1's command line.
    let view = |id: &BoxId| {
        node.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(dir.clone()),
            owner: None,
            band: 0,
        })
        .expect("create a box with masks beneath /proc");
        node.start(id).expect("start");
        let (_, mi) = run_in(&node, id, &["/usr/bin/cat", "/proc/self/mountinfo"]);
        let (_, kcore) = run_in(&node, id, &["/usr/bin/wc", "-c", "/proc/kcore"]);
        let (_, acpi) = run_in(&node, id, &["/usr/bin/ls", "-A", "/proc/acpi"]);
        let sysrq = exit_in(
            &node,
            id,
            &["/usr/bin/sh", "-c", "echo h > /proc/sysrq-trigger"],
        );
        let (_, init) = run_in(&node, id, &["/usr/bin/cat", "/proc/1/cmdline"]);
        node.delete(id).expect("delete");
        let beneath: BTreeSet<(String, bool)> = mountinfo(&mi)
            .into_iter()
            .filter(|(point, ..)| point.starts_with("/proc/"))
            .map(|(point, opts, ..)| (point, opts.split(',').any(|o| o == "ro")))
            .collect();
        (beneath, kcore, acpi, sysrq, init)
    };
    // Below Linux 7.1 the kernel won't hand over a pid namespace with no process in it, so the node makes
    // none ahead: its pool stops at its first try, and both boxes are made the ordinary way.
    let ahead = kernel_at_least(7, 1);
    let before = node.proc_mounts().unwrap();
    let first = view(&BoxId::new("procs-a").unwrap());
    wait_for(
        &AHEAD,
        || {
            let c = node.proc_mounts().unwrap();
            if ahead {
                c.made > before.made
            } else {
                c.failed_namespace > before.failed_namespace
            }
        },
        "the node's first try at a proc made ahead",
    );
    let taken = node.proc_mounts().unwrap().taken;
    let second = view(&BoxId::new("procs-b").unwrap());
    let after = node.proc_mounts().unwrap();
    if ahead {
        assert_eq!(
            after.taken,
            taken + 1,
            "the second box's proc was made ahead"
        );
    } else {
        assert_eq!(after.taken, taken, "no proc made ahead below 7.1");
        assert_eq!(
            after.disabled,
            before.disabled + 1,
            "the second create counted disabled"
        );
    }
    assert_eq!(node.creates_through_shim(), 0, "both boxes made pruned");
    for (which, (beneath, kcore, acpi, sysrq, init)) in [("first", &first), ("second", &second)] {
        for point in ["/proc/kcore", "/proc/keys", "/proc/timer_list"] {
            assert!(
                beneath.iter().any(|(p, _)| p == point),
                "{which}: {point} is masked: {beneath:?}"
            );
        }
        for point in ["/proc/sys", "/proc/sysrq-trigger", "/proc/bus"] {
            assert!(
                beneath.contains(&(point.to_owned(), true)),
                "{which}: {point} is read-only: {beneath:?}"
            );
        }
        assert!(
            !beneath.iter().any(|(p, _)| p == "/proc/not-there"),
            "{which}: a missing path is skipped"
        );
        assert_eq!(kcore.split_whitespace().next(), Some("0"), "{which}: kcore");
        assert_eq!(acpi.trim(), "", "{which}: a masked directory is empty");
        assert_ne!(
            *sysrq,
            Exit::Code(0),
            "{which}: a read-only path refuses a write"
        );
        assert!(
            init.contains("selftest-box"),
            "{which}: pid 1 is the box's init: {init:?}"
        );
    }
    assert_eq!(
        first.0, second.0,
        "the made-ahead proc's mounts are the ordinary one's"
    );
    eprintln!(
        "procs made ahead: taken {}, made {}, the masks and read-only paths as ordinary setup's",
        after.taken, after.made
    );
}

/// Decision 8, parked procs (AC-T7.1.3; T-T7.1.4): on a root node made with parked procs and one cgroup made ahead
/// for boxes of no owner, procs made ahead off, so the only procs the node holds are its parked inits'. Do: create
/// and delete N0, which mounts no proc, so the init parked next has none; create N1 with key K: expect it to claim
/// that init, counted dry. Delete N1, whose spec masks and makes read-only paths beneath `/proc` (key K), so the init parked next has a
/// proc made for K. Create N2 with K. Expect: before N2, the node alone holds that proc (this process has one proc
/// root open, the parked init none); N2's init is the parked one, its `/proc` that proc's superblock, which the
/// node holds no more; in N2 `/proc/self` is the reader's own pid and pid 1 the box's init; its mounts beneath
/// `/proc` exactly K's, the read-only ones read-only; one hit counted. Then delete N2 (the next park's proc made for
/// K; its slot removed with it) and create N3 with another key, K2. Expect: N3's init the parked one, claimed, but its
/// `/proc` its own, its mounts exactly K2's, which K's tree doesn't carry; one miss counted. Then delete N3 and release the idle cgroups:
/// expect no proc held and no proc root open in this process, the parked init ended. Each proc no init attached (N3's
/// miss, the last one released) is dropped by the node's dropper thread, never on a create's: its unmount waits on
/// the host's `namespace_sem` and an RCU grace period. None failed to be made.
fn a_parked_inits_proc_is_its_own_and_taken_once(lab: &Lab, own: &Path) {
    if lab.rootless {
        return;
    }
    use std::os::unix::fs::MetadataExt;
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab
        .node(&own.join("hex-oci-parkproc"), state.path())
        .with_prepared_procs(false)
        .unwrap()
        .with_inits_ahead(1);
    let k = (
        &[
            "/proc/kcore",
            "/proc/keys",
            "/proc/timer_list",
            "/proc/acpi",
            "/proc/not-there",
        ][..],
        &["/proc/sys", "/proc/sysrq-trigger", "/proc/bus"][..],
    );
    let k2 = (&["/proc/kcore", "/proc/keys"][..], &["/proc/sys"][..]);
    let bundle = |(masked, readonly): (&[&str], &[&str])| {
        let dir = tempfile::Builder::new()
            .prefix("bundle-")
            .tempdir_in(lab.bundles.path())
            .unwrap()
            .keep();
        let paths = format!(r#","maskedPaths":{masked:?},"readonlyPaths":{readonly:?}"#);
        std::fs::write(
            dir.join("config.json"),
            lab.config_ex(
                &Lab::act(&Act::Wait),
                r#"{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"}"#,
                "",
                "",
                "",
                &paths,
            ),
        )
        .unwrap();
        dir
    };
    let (with_k, with_k2) = (bundle(k), bundle(k2));
    // A box with no proc of its own: no proc made ahead can serve it.
    let no_proc = {
        let dir = tempfile::Builder::new()
            .prefix("bundle-")
            .tempdir_in(lab.bundles.path())
            .unwrap()
            .keep();
        let config = lab.config_ex(
            &Lab::act(&Act::Wait),
            r#"{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"}"#,
            "",
            "",
            "",
            "",
        );
        let proc_mount = r#"{"destination": "/proc", "type": "proc", "source": "proc"},"#;
        assert!(config.contains(proc_mount));
        std::fs::write(dir.join("config.json"), config.replace(proc_mount, "")).unwrap();
        dir
    };
    let create = |id: &BoxId, dir: &Path| {
        node.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(dir.to_path_buf()),
            owner: None,
            band: 0,
        })
        .expect("create a box of no owner with masks beneath /proc");
    };
    // The mounts a key's box has beneath `/proc`: each path the kernel makes, read-only where it says so.
    let expected = |(masked, readonly): (&[&str], &[&str])| -> BTreeSet<String> {
        masked
            .iter()
            .chain(readonly)
            .filter(|p| Path::new(p).exists())
            .map(|p| (*p).to_owned())
            .collect()
    };
    // What a box sees beneath its proc, and whether `/proc/self` is the reader (the shell opens it itself, by a
    // redirection, so it names the shell) and pid 1 its init.
    let seen = |id: &BoxId| {
        let (_, mi) = run_in(&node, id, &["/usr/bin/cat", "/proc/self/mountinfo"]);
        let beneath: BTreeSet<(String, bool)> = mountinfo(&mi)
            .into_iter()
            .filter(|(point, ..)| point.starts_with("/proc/"))
            .map(|(point, opts, ..)| (point, opts.split(',').any(|o| o == "ro")))
            .collect();
        let own_pidns = exit_in(
            &node,
            id,
            &[
                "/usr/bin/sh",
                "-c",
                r#"read -r me _ < /proc/self/stat && [ "$me" = "$$" ] && [ "$(cat /proc/1/comm)" = hex ]"#,
            ],
        );
        (beneath, own_pidns)
    };
    let check = |which: &str, beneath: &BTreeSet<(String, bool)>, key: (&[&str], &[&str])| {
        let points: BTreeSet<String> = beneath.iter().map(|(p, _)| p.clone()).collect();
        assert_eq!(
            points,
            expected(key),
            "{which}: its mounts beneath /proc are its key's"
        );
        for point in key.1.iter().filter(|p| Path::new(p).exists()) {
            assert!(
                beneath.contains(&((*point).to_owned(), true)),
                "{which}: {point} is read-only: {beneath:?}"
            );
        }
    };
    let ready = |made: u64| {
        wait_for(
            &AHEAD,
            || {
                let c = node.cgroup_reuse().unwrap();
                c.ahead.ready == 1 && c.parked.procs_made + c.parked.procs_failed >= made
            },
            "an init parked ahead with its proc",
        );
    };
    let me = std::process::id().to_string();
    let init_of = |id: &BoxId| {
        let procs =
            std::fs::read_to_string(node.cgroup_dir(id).unwrap().join("cgroup.procs")).unwrap();
        procs.lines().next().unwrap().trim().to_owned()
    };
    let proc_dev = |pid: &str| {
        std::fs::metadata(format!("/proc/{pid}/root/proc"))
            .unwrap()
            .dev()
    };

    // N0 has no proc, so the init parked after it has none made; N1, with K, claims it: dry.
    let n0 = BoxId::new("parkproc-0").unwrap();
    create(&n0, &no_proc);
    let root = node
        .cgroup_dir(&n0)
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    node.delete(&n0).unwrap();
    ready(0);
    let start = node.cgroup_reuse().unwrap().parked;
    assert_eq!(
        start.procs_made, 0,
        "no proc made for a box with none: {start:?}"
    );
    let n1 = BoxId::new("parkproc-1").unwrap();
    create(&n1, &with_k);
    let dry = node.cgroup_reuse().unwrap().parked;
    assert_eq!(
        (dry.claimed, dry.proc_dry, dry.proc_hits, dry.proc_misses),
        (start.claimed + 1, start.proc_dry + 1, 0, 0),
        "N1 claimed the parked init, which had no proc for K: {dry:?}"
    );
    node.delete(&n1).unwrap();
    ready(1);
    let before = node.cgroup_reuse().unwrap().parked;
    assert_eq!(
        (before.procs_made, before.procs_failed, before.procs_held),
        (1, 0, 1),
        "{before:?}"
    );
    // A box of no owner's delete parks nothing: the hex-ahead refill parked this init and made its proc.
    assert_eq!(
        (before.procs_made_at_refill, before.procs_made_at_delete),
        (1, 0),
        "the refill made it: {before:?}"
    );
    let held = held_procs(&me);
    assert_eq!(
        held.len(),
        1,
        "the node holds the parked init's proc: {held:?}"
    );
    let parked = parked_inits(&root);
    assert_eq!(parked.len(), 1, "one init parked ahead: {parked:?}");
    assert_eq!(
        held_procs(&parked[0]),
        BTreeSet::new(),
        "the parked init holds no proc of its own before its claim"
    );

    let n2 = BoxId::new("parkproc-2").unwrap();
    create(&n2, &with_k);
    let after = node.cgroup_reuse().unwrap().parked;
    assert_eq!(init_of(&n2), parked[0], "N2's init is the parked one");
    assert_eq!(after.claimed, before.claimed + 1, "{after:?}");
    assert_eq!(
        after.proc_hits,
        before.proc_hits + 1,
        "N2 took the parked proc: {after:?}"
    );
    assert_eq!(after.proc_misses, before.proc_misses, "{after:?}");
    node.start(&n2).unwrap();
    assert!(
        held.contains(&proc_dev(&parked[0])),
        "N2's /proc is the proc made for its parked init"
    );
    assert!(
        !held_procs(&me).iter().any(|d| held.contains(d)),
        "the node holds that proc no more: it was handed to one init"
    );
    let (beneath, own_pidns) = seen(&n2);
    check("N2", &beneath, k);
    assert_eq!(
        own_pidns,
        Exit::Code(0),
        "N2's /proc shows its own pid namespace"
    );

    // N2's create told the refill K; the next init parked ahead has its proc made for K. N2 goes first, its slot
    // with it, so the only slot left holds the parked init.
    ready(2);
    node.delete(&n2).unwrap();
    let before = node.cgroup_reuse().unwrap().parked;
    let parked = parked_inits(&root);
    assert_eq!(parked.len(), 1, "one init parked ahead: {parked:?}");
    let n3 = BoxId::new("parkproc-3").unwrap();
    create(&n3, &with_k2);
    let after = node.cgroup_reuse().unwrap().parked;
    assert_eq!(init_of(&n3), parked[0], "N3's init is the parked one");
    assert_eq!(after.claimed, before.claimed + 1, "{after:?}");
    assert_eq!(
        after.proc_misses,
        before.proc_misses + 1,
        "N3's key isn't K: {after:?}"
    );
    assert_eq!(after.proc_hits, before.proc_hits, "{after:?}");
    // The missed tree goes to the node's dropper, never unmounted on N3's create: queued by the create, dropped by
    // the dropper, none inline.
    assert_eq!(
        (after.procs_queued, after.procs_dropped_inline),
        (before.procs_queued + 1, before.procs_dropped_inline),
        "N3's create queued its missed proc: {after:?}"
    );
    wait_for(
        &DROPPED,
        || node.cgroup_reuse().unwrap().parked.procs_dropped == before.procs_dropped + 1,
        "the missed proc dropped by the node's dropper",
    );
    node.start(&n3).unwrap();
    // N3's /proc is its own, not the one made for K: its mounts are exactly K2's, which K's tree doesn't carry. (Its
    // `st_dev` says nothing here: a proc's is an anonymous device number, which the kernel hands out again once the
    // missed tree's superblock is freed.)
    let (beneath, own_pidns) = seen(&n3);
    check("N3", &beneath, k2);
    assert_eq!(
        own_pidns,
        Exit::Code(0),
        "N3's /proc shows its own pid namespace"
    );

    ready(3);
    node.delete(&n3).unwrap();
    let parked = parked_inits(&root);
    assert_eq!(parked.len(), 1, "one init parked ahead: {parked:?}");
    let last = rustix::process::pidfd_open(
        rustix::process::Pid::from_raw(parked[0].parse().unwrap()).unwrap(),
        rustix::process::PidfdFlags::empty(),
    )
    .unwrap();
    node.release_idle().unwrap();
    // Every proc no init attached is dropped by the dropper, none where it was.
    wait_for(
        &DROPPED,
        || {
            let c = node.cgroup_reuse().unwrap().parked;
            c.procs_dropped + c.procs_dropped_inline == c.procs_made - c.proc_hits
        },
        "every unattached proc dropped",
    );
    let end = node.cgroup_reuse().unwrap().parked;
    assert_eq!(end.procs_held, 0, "released, no proc is held: {end:?}");
    assert_eq!(
        end.procs_dropped_inline, 0,
        "none dropped on its caller's thread: {end:?}"
    );
    assert_eq!(
        held_procs(&me),
        BTreeSet::new(),
        "no proc root is open in the node"
    );
    wait_for(
        &KILLED,
        || {
            let mut fds = [rustix::event::PollFd::new(
                &last,
                rustix::event::PollFlags::IN,
            )];
            let now = rustix::event::Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            };
            rustix::event::poll(&mut fds, Some(&now)).unwrap() > 0
        },
        "the last parked init to end",
    );
    assert_eq!(end.procs_failed, 0, "{end:?}");
    eprintln!(
        "parked procs: made {}, hits {}, misses {}, held after release {}",
        end.procs_made, end.proc_hits, end.proc_misses, end.procs_held
    );
}

/// Decision 8, parked procs, on 7.x's ways (AC-T7.1.3; T-T7.1.4): an init parked for a node whose mount namespaces
/// are made from the tree (Linux 7.0) or empty (7.1) is cloned with that way's flags, so it takes the way's creates,
/// its proc with it. Do, on a root node of each way the kernel has, with one cgroup made ahead and parked procs on:
/// create and delete N1 with masks beneath `/proc` (key K), wait for an init parked with its proc, create N2 with K.
/// Expect: N2's init the parked one, claimed; one hit; one mount namespace made the node's way, counted; in N2 its
/// mounts beneath `/proc` exactly K's and `/proc/self` the reader. A way the kernel lacks is skipped, said.
fn a_parked_init_takes_a_create_made_each_way(lab: &Lab, own: &Path) {
    use hex_runtime::linux::oci::mntns::{self, MountNamespace, OpenTreeNamespace};
    if lab.rootless {
        return;
    }
    let tree_here = mntns::probe().expect("probe open_tree(OPEN_TREE_NAMESPACE)")
        == OpenTreeNamespace::Available;
    let empty_here = kernel_at_least(7, 1);
    let paths = r#","maskedPaths":["/proc/kcore","/proc/keys","/proc/timer_list"],"readonlyPaths":["/proc/sys","/proc/sysrq-trigger"]"#;
    let dir = tempfile::Builder::new()
        .prefix("bundle-")
        .tempdir_in(lab.bundles.path())
        .unwrap()
        .keep();
    std::fs::write(
        dir.join("config.json"),
        lab.config_ex(
            &Lab::act(&Act::Wait),
            r#"{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"}"#,
            "",
            "",
            "",
            paths,
        ),
    )
    .unwrap();
    for (asked, way, here) in [
        ("tree", MountNamespace::FromTree, tree_here),
        ("empty", MountNamespace::Empty, empty_here),
    ] {
        if !here {
            eprintln!("SKIP parked init, way {asked}: this kernel hasn't it");
            continue;
        }
        let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
        let node = Node::open(
            &own.join(format!("hex-oci-parkway-{asked}")),
            state.path(),
            state.path(),
        )
        .expect("open a node")
        // The way is set before the seeds are built, which with_oci_helper does.
        .with_mount_namespace(asked)
        .with_oci_helper(PathBuf::from(&lab.exe))
        .with_prepared_procs(false)
        .unwrap()
        .with_inits_ahead(1);
        let create = |id: &BoxId| {
            node.create(BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: id.clone(),
                bounds: Bounds::default(),
                class: ClassSpec::Bundle(dir.clone()),
                owner: None,
                band: 0,
            })
            .expect("create a box of no owner")
        };
        let n1 = BoxId::new(&format!("parkway-{asked}-1")).unwrap();
        create(&n1);
        let root = node
            .cgroup_dir(&n1)
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        node.delete(&n1).unwrap();
        wait_for(
            &AHEAD,
            || {
                let c = node.cgroup_reuse().unwrap();
                c.ahead.ready == 1 && c.parked.procs_made + c.parked.procs_failed >= 1
            },
            "an init parked ahead with its proc",
        );
        let before = node.cgroup_reuse().unwrap().parked;
        assert_eq!(
            (before.procs_made, before.procs_failed),
            (1, 0),
            "{asked}: {before:?}"
        );
        let parked = parked_inits(&root);
        assert_eq!(
            parked.len(),
            1,
            "{asked}: one init parked ahead: {parked:?}"
        );
        let made_before = node.mount_namespaces_made(way);
        let n2 = BoxId::new(&format!("parkway-{asked}-2")).unwrap();
        create(&n2);
        let after = node.cgroup_reuse().unwrap().parked;
        let procs =
            std::fs::read_to_string(node.cgroup_dir(&n2).unwrap().join("cgroup.procs")).unwrap();
        assert_eq!(
            procs.lines().next().map(str::trim),
            Some(parked[0].as_str()),
            "{asked}: N2's init is the parked one"
        );
        assert_eq!(
            (after.claimed, after.proc_hits),
            (before.claimed + 1, before.proc_hits + 1),
            "{asked}: {after:?}"
        );
        assert_eq!(
            node.mount_namespaces_made(way),
            made_before + 1,
            "{asked}: N2's mount namespace made the node's way"
        );
        node.start(&n2).unwrap();
        let (_, mi) = run_in(&node, &n2, &["/usr/bin/cat", "/proc/self/mountinfo"]);
        let beneath: BTreeSet<String> = mountinfo(&mi)
            .into_iter()
            .map(|(point, ..)| point)
            .filter(|point| point.starts_with("/proc/"))
            .collect();
        let want: BTreeSet<String> = [
            "/proc/kcore",
            "/proc/keys",
            "/proc/timer_list",
            "/proc/sys",
            "/proc/sysrq-trigger",
        ]
        .into_iter()
        .filter(|p| Path::new(p).exists())
        .map(str::to_owned)
        .collect();
        assert_eq!(beneath, want, "{asked}: N2's mounts beneath /proc are K's");
        assert_eq!(
            exit_in(
                &node,
                &n2,
                &[
                    "/usr/bin/sh",
                    "-c",
                    r#"read -r me _ < /proc/self/stat && [ "$me" = "$$" ]"#
                ],
            ),
            Exit::Code(0),
            "{asked}: N2's /proc shows its own pid namespace"
        );
        node.delete(&n2).unwrap();
        node.release_idle().unwrap();
        eprintln!("parked init, way {asked}: N2 claimed it, its proc a hit");
    }
}

/// The proc superblocks (`st_dev`) whose root process `pid` holds a descriptor to, read through `/proc/<pid>/fd`:
/// each link followed to what it names, a proc root (`PROC_SUPER_MAGIC`, proc(5); its root inode 1).
fn held_procs(pid: &str) -> BTreeSet<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::read_dir(format!("/proc/{pid}/fd"))
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|e| {
            let path = e.path();
            let fs = rustix::fs::statfs(&path).ok()?;
            let meta = std::fs::metadata(&path).ok()?;
            (u64::try_from(fs.f_type).ok() == Some(0x9fa0) && meta.ino() == 1).then(|| meta.dev())
        })
        .collect()
}

/// The processes of the slot cgroups under `root`: the inits parked in those made ahead.
fn parked_inits(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(root).unwrap().filter_map(Result::ok) {
        if entry
            .file_name()
            .to_str()
            .is_some_and(|n| hex_core::reuse::slot_of(n).is_some())
        {
            let procs =
                std::fs::read_to_string(entry.path().join("cgroup.procs")).unwrap_or_default();
            out.extend(procs.lines().map(|l| l.trim().to_owned()));
        }
    }
    out
}

/// HEX-MNTNS-8 (decision 6's update; T-T7.1.4): on a root node, make two boxes on the pruned path and two through
/// the shim (a createRuntime hook), each masking Docker's directories beneath `/proc`, with procs made ahead off
/// so each init masks them itself. Expect every masked directory of all four boxes on one filesystem, the node's
/// one empty read-only tmpfs, as crun binds one empty directory: no superblock made per masked directory. Where
/// the kernel won't clone a detached mount (before Linux 6.15, v6.8 `fs/namespace.c:2588`), expect each box made
/// all the same, each masked directory an empty read-only tmpfs of its own.
fn masked_directories_are_the_nodes_one_empty_filesystem(lab: &Lab, own: &Path) {
    if lab.rootless {
        return;
    }
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab
        .node(&own.join("hex-oci-maskdirs"), state.path())
        .with_prepared_procs(false)
        .expect("procs made ahead off");
    let paths = r#","maskedPaths":["/proc/acpi","/proc/asound","/proc/scsi","/proc/kcore"]"#;
    let hooks = r#","hooks":{"createRuntime":[{"path":"/usr/bin/true","args":["true"]}]}"#;
    let mut devs = BTreeSet::new();
    let mut masked = 0;
    // Each box's masked directories' devices, apart: a box's filesystems end at its delete, and the kernel gives
    // a freed anonymous device's number to the next one it makes, so only within a box do distinct ones show.
    let mut each_box: Vec<(usize, BTreeSet<String>)> = Vec::new();
    for (n, (path, hook)) in [
        ("pruned", ""),
        ("pruned", ""),
        ("shim", hooks),
        ("shim", hooks),
    ]
    .into_iter()
    .enumerate()
    {
        let dir = tempfile::Builder::new()
            .prefix("bundle-")
            .tempdir_in(lab.bundles.path())
            .unwrap()
            .keep();
        std::fs::write(
            dir.join("config.json"),
            lab.config_ex(
                &Lab::act(&Act::Wait),
                r#"{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"}"#,
                "",
                "",
                hook,
                paths,
            ),
        )
        .unwrap();
        let id = BoxId::new(&format!("maskdirs-{n}")).unwrap();
        let shim = node.creates_through_shim();
        node.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(dir),
            owner: None,
            band: 0,
        })
        .expect("create a box masking directories");
        assert_eq!(
            node.creates_through_shim() - shim,
            u64::from(path == "shim"),
            "box {n} made {path}"
        );
        node.start(&id).expect("start");
        let (_, mi) = run_in(&node, &id, &["/usr/bin/cat", "/proc/self/mountinfo"]);
        node.delete(&id).expect("delete");
        let (mut here, mut its) = (0, BTreeSet::new());
        // proc_pid_mountinfo(5): the third field is the filesystem's major:minor, the fifth the mount point.
        for line in mi.lines() {
            let fields: Vec<&str> = line.split(' ').collect();
            if let (Some(dev), Some(point)) = (fields.get(2), fields.get(4))
                && ["/proc/acpi", "/proc/asound", "/proc/scsi"].contains(point)
            {
                // The fields after " - ": the filesystem's type, its source, its superblock's options.
                let fs = line.split(" - ").nth(1).unwrap_or_default();
                assert!(
                    fs.starts_with("tmpfs ") && fields.get(5).is_some_and(|o| o.starts_with("ro")),
                    "{point} in box {n} is masked by a read-only tmpfs: {line}"
                );
                devs.insert((*dev).to_owned());
                its.insert((*dev).to_owned());
                masked += 1;
                here += 1;
            }
        }
        each_box.push((here, its));
    }
    assert!(masked >= 4, "masked directories to compare: {masked}");
    if kernel_clones_a_detached_mount() {
        assert_eq!(
            devs.len(),
            1,
            "one empty filesystem for every masked directory of every box: {devs:?}; by box {each_box:?}; parked {:?}",
            node.cgroup_reuse().map(|c| c.parked)
        );
    } else {
        for (n, (here, its)) in each_box.iter().enumerate() {
            assert_eq!(
                its.len(),
                *here,
                "a tmpfs of its own for every masked directory of box {n}: {its:?}"
            );
        }
    }
    eprintln!("masked directories: {masked} across 4 boxes, on {devs:?}");
}

/// Goal 6, the seed without a `pivot_root` per start (`goal6-mntns.md` §12, AC-T7.1.4's under-load bar): a node
/// prunes one mount namespace and every create seed joins it by `setns`, so a seed started after the first never
/// pivots (each `pivot_root` walks every thread on the host under the mount lock, §11). Do: make a first box, then
/// pairs of boxes at once until a second create seed is up, then read each live create seed's mount namespace from
/// this process's `/proc`. Expect at least two create seeds, every one in the same mount namespace and not this
/// process's, and that namespace holding the pruned two mounts after the concurrent creates as after the first: no
/// seed or init ever mutates it.
fn create_seeds_share_the_nodes_one_pruned_namespace(lab: &Lab, own: &Path) {
    use std::os::unix::fs::MetadataExt;
    if lab.rootless {
        return;
    }
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-seedns"), state.path());
    // A create seed is a child of this process re-exec'd as `__oci-seed` that joins the node's pruned namespace,
    // as its environment says; an exec seed doesn't. A box's init
    // made and not yet started is a child of this process too (cloned with `CLONE_PARENT`) and still the seed's
    // image, but in its box's pid namespace: its `NSpid` names two pids, a seed's one (proc_pid_status(5)).
    let create_seeds = || -> Vec<u32> {
        let me = std::process::id().to_string();
        let mut seeds = Vec::new();
        for entry in std::fs::read_dir("/proc").unwrap().flatten() {
            let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
                continue;
            };
            let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap_or_default();
            let ppid = stat.rsplit(") ").next().and_then(|r| r.split(' ').nth(1));
            let cmdline = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
            let environ = std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default();
            let has = |hay: &[u8], needle: &[u8]| hay.windows(needle.len()).any(|w| w == needle);
            let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap_or_default();
            let in_our_pid_namespace = status
                .lines()
                .find_map(|l| l.strip_prefix("NSpid:"))
                .is_some_and(|pids| pids.split_whitespace().count() == 1);
            if ppid == Some(me.as_str())
                && in_our_pid_namespace
                && has(&cmdline, b"__oci-seed")
                && has(&environ, b"HEX_OCI_SEED_JOIN=")
            {
                seeds.push(pid);
            }
        }
        seeds
    };
    let namespace = |pid: u32| {
        std::fs::metadata(format!("/proc/{pid}/ns/mnt"))
            .map(|m| m.ino())
            .ok()
    };
    let mounts = |pid: u32| {
        std::fs::read_to_string(format!("/proc/{pid}/mountinfo"))
            .map(|t| t.lines().count())
            .ok()
    };
    let create = |id: &BoxId| {
        let dir = tempfile::Builder::new()
            .prefix("bundle-")
            .tempdir_in(lab.bundles.path())
            .unwrap()
            .keep();
        std::fs::write(
            dir.join("config.json"),
            lab.config(&Lab::act(&Act::Wait), false),
        )
        .unwrap();
        node.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(dir),
            owner: None,
            band: 0,
        })
        .expect("create a box through a seed");
    };
    let first = BoxId::new("seedns-first").unwrap();
    // The first create is answered by a live seed, so one is up to read the namespace of before the rest.
    create(&first);
    let before = create_seeds()
        .into_iter()
        .find_map(mounts)
        .expect("a create seed's mount table after the first create");
    // Then pairs of creates at once until a second seed is up (the fact the comparison needs), at most one pair
    // for each of the pool's slots (the node's `seeds`, `Node::sizes`), not a count picked to make one likely.
    let mut ids = vec![first];
    for round in 0..node.sizes().seeds.get() {
        if create_seeds().len() >= 2 {
            break;
        }
        let pair: Vec<BoxId> = (0..2)
            .map(|k| BoxId::new(&format!("seedns-{round}-{k}")).unwrap())
            .collect();
        std::thread::scope(|s| {
            for id in &pair {
                s.spawn(|| create(id));
            }
        });
        ids.extend(pair);
    }
    let seeds = create_seeds();
    let spaces: BTreeSet<u64> = seeds.iter().filter_map(|&p| namespace(p)).collect();
    let tables: BTreeSet<usize> = seeds.iter().filter_map(|&p| mounts(p)).collect();
    for id in &ids {
        node.delete(id).expect("delete");
    }
    assert!(
        seeds.len() >= 2,
        "several create seeds to compare: {seeds:?}"
    );
    assert_eq!(
        spaces.len(),
        1,
        "every create seed in one mount namespace: {spaces:?}"
    );
    let own_space = std::fs::metadata("/proc/self/ns/mnt").unwrap().ino();
    assert!(
        !spaces.contains(&own_space),
        "the seeds' namespace is pruned, not the node's own"
    );
    assert_eq!(
        tables,
        BTreeSet::from([before]),
        "the pruned namespace unchanged by the creates"
    );
    assert!(
        before <= 2,
        "the pruned namespace holds its tmpfs root and no more: {before} mounts"
    );
    eprintln!(
        "create seeds: {} in one pruned namespace of {before} mounts",
        seeds.len()
    );
}

/// Whether `open_tree(OPEN_TREE_CLONE)` clones a mount outside the caller's namespace, a detached tmpfs's: Linux
/// 6.15's `may_copy_tree` admits it (v6.19 `fs/namespace.c:2928-2948`), v6.8 refuses it with `EINVAL` (`:2588`).
fn kernel_clones_a_detached_mount() -> bool {
    use rustix::mount::{FsMountFlags, FsOpenFlags, MountAttrFlags, OpenTreeFlags};
    use std::os::fd::AsFd;
    let fs = rustix::mount::fsopen("tmpfs", FsOpenFlags::FSOPEN_CLOEXEC).expect("fsopen tmpfs");
    rustix::mount::fsconfig_create(fs.as_fd()).expect("make the tmpfs");
    let mount = rustix::mount::fsmount(
        fs.as_fd(),
        FsMountFlags::FSMOUNT_CLOEXEC,
        MountAttrFlags::empty(),
    )
    .expect("mount the tmpfs, detached");
    match rustix::mount::open_tree(
        &mount,
        "",
        OpenTreeFlags::OPEN_TREE_CLONE
            | OpenTreeFlags::AT_EMPTY_PATH
            | OpenTreeFlags::OPEN_TREE_CLOEXEC,
    ) {
        Ok(_) => true,
        Err(rustix::io::Errno::INVAL) => false,
        Err(e) => panic!("cloning a detached tmpfs: {e}"),
    }
}

/// Goal 5 plan step 2 (T-T7.1.4): a spec binding `/usr` read-only and a volume. Both are in the
/// container's mountinfo with the options asked, and the bundle's config.json and the node's state
/// directory are unreachable from inside — the pruned root holds nothing of the host but what the spec
/// mounts. Root only: a pruned seed is a root node's, and the pruned path is what keeps the host out.
fn a_bind_and_a_volume_are_present_the_host_unreachable(lab: &Lab, own: &Path) {
    if lab.rootless {
        return;
    }
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-vol"), state.path());
    let vol = lab.bundles.path().join("volume");
    std::fs::create_dir_all(&vol).unwrap();
    std::fs::write(vol.join("marker"), "from the volume\n").unwrap();
    let volume = format!(
        ",\n    {{\"destination\": \"/vol\", \"type\": \"bind\", \"source\": \"{}\", \"options\": [\"rbind\",\"rw\"]}}",
        vol.display()
    );
    let dir = tempfile::Builder::new()
        .prefix("bundle-")
        .tempdir_in(lab.bundles.path())
        .unwrap()
        .keep();
    std::fs::write(
        dir.join("config.json"),
        lab.config_with(
            &Lab::act(&Act::Wait),
            r#"{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"}"#,
            "",
            &volume,
        ),
    )
    .unwrap();
    let id = BoxId::new("vol").unwrap();
    node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Bundle(dir.clone()),
        owner: None,
        band: 0,
    })
    .expect("create a box with a read-only bind and a volume");
    node.start(&id).expect("start");
    let pruned = node.pruned_creates();
    // Gather what the box sees before deleting, so a failed check leaves no box running.
    let (mi_exit, mi) = run_in(&node, &id, &["/usr/bin/cat", "/proc/self/mountinfo"]);
    let (vol_exit, vol_out) = run_in(&node, &id, &["/usr/bin/cat", "/vol/marker"]);
    let config = dir.join("config.json").to_string_lossy().into_owned();
    let state_path = state.path().to_string_lossy().into_owned();
    let config_reach = exit_in(&node, &id, &["/usr/bin/test", "-e", &config]);
    let state_reach = exit_in(&node, &id, &["/usr/bin/test", "-e", &state_path]);
    node.delete(&id).expect("delete");

    assert!(
        pruned >= 1,
        "the bind-and-volume box is made on the pruned path, {pruned} pruned"
    );
    assert_eq!(mi_exit, Exit::Code(0), "read the box's mountinfo: {mi}");
    let mounts = mountinfo(&mi);
    let usr = mounts
        .iter()
        .rev()
        .find(|(point, ..)| point == "/usr")
        .expect("/usr in the box");
    assert!(
        usr.1.split(',').any(|o| o == "ro"),
        "/usr is read-only: {usr:?}"
    );
    let vol_mount = mounts
        .iter()
        .rev()
        .find(|(point, ..)| point == "/vol")
        .expect("/vol in the box");
    assert!(
        vol_mount.1.split(',').any(|o| o == "rw"),
        "/vol is read-write: {vol_mount:?}"
    );
    assert_eq!(
        (vol_exit, vol_out.trim()),
        (Exit::Code(0), "from the volume"),
        "the volume's content is there"
    );
    assert_ne!(
        config_reach,
        Exit::Code(0),
        "the bundle's config.json is unreachable from inside the box"
    );
    assert_ne!(
        state_reach,
        Exit::Code(0),
        "the node's state directory is unreachable from inside the box"
    );
    eprintln!(
        "bind+volume: /usr read-only and /vol read-write are in the box; config.json and the state dir are unreachable"
    );
}

/// Whether this kernel's release is at least `major.minor`, read from `/proc/sys/kernel/osrelease`, and said.
fn kernel_at_least(major: u32, minor: u32) -> bool {
    let release = std::fs::read_to_string("/proc/sys/kernel/osrelease").unwrap();
    let mut v = release
        .trim()
        .split(['.', '-'])
        .map(|p| p.parse::<u32>().unwrap_or(0));
    let here = (v.next().unwrap(), v.next().unwrap());
    eprintln!("kernel {}", release.trim());
    here >= (major, minor)
}

/// `argv` in box `id` through the seam, its exit and stdout once it ends.
fn run_in(rt: &dyn Runtime, id: &BoxId, argv: &[&str]) -> (Exit, String) {
    let (mut out, w) = std::io::pipe().unwrap();
    let mut proc = rt
        .exec(
            id,
            hex_api::Program::of(argv),
            hex_api::Stdio {
                stdout: Some(w),
                ..hex_api::Stdio::null()
            },
        )
        .expect("exec in the box");
    let mut s = String::new();
    std::io::Read::read_to_string(&mut out, &mut s).unwrap();
    let exit = proc
        .wait_for(conformance::PATIENCE)
        .unwrap()
        .expect("the program ends");
    (exit, s)
}

/// `argv` in box `id`, its exit alone.
fn exit_in(rt: &dyn Runtime, id: &BoxId, argv: &[&str]) -> Exit {
    run_in(rt, id, argv).0
}

/// Each `/proc/<pid>/mountinfo` line as (mount point, per-mount options, optional/propagation fields,
/// filesystem type) (proc_pid_mountinfo(5)); tuples, so an unread field is no dead-code warning.
fn mountinfo(text: &str) -> Vec<(String, String, String, String)> {
    text.lines()
        .filter_map(|line| {
            let (left, right) = line.split_once(" - ")?;
            let left: Vec<&str> = left.split(' ').collect();
            Some((
                (*left.get(4)?).to_owned(),
                (*left.get(5)?).to_owned(),
                left.get(6..).map(|o| o.join(" ")).unwrap_or_default(),
                right.split(' ').next().unwrap_or("").to_owned(),
            ))
        })
        .collect()
}

/// The mount points in this process's (the agent's) own mount namespace, as a set, so a failed create
/// can be shown to have left none of a container's mounts behind (goal 5 plan step 2).
fn agent_mount_points() -> std::collections::BTreeSet<String> {
    mountinfo(&std::fs::read_to_string("/proc/self/mountinfo").expect("read the agent's mountinfo"))
        .into_iter()
        .map(|(point, ..)| point)
        .collect()
}

/// Goal 5 plan step 2 (T-T7.1.4): creates that can't take the pruned path take the classic path, each
/// counted under its reason. Rootless, where the node's seeds don't prune: a create is counted under
/// `Rootless`, made by a seed (not through the shim) and not pruned. As root, specs a pruned seed can't
/// serve: a user namespace of its own (`UserNamespace`), createContainer hooks (`Hooks`) and a mount
/// namespace joined by path (`JoinsByPath`), each through the shim, and none counted under `Rootless`.
/// A create routes classic at create — the counter bumps there, before the box runs — so creating then
/// deleting each is enough to count it.
fn classic_reasons_are_counted(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-reasons"), state.path());
    if lab.rootless {
        let id = BoxId::new("reason-rootless").unwrap();
        let before = node.classic_creates_for(ClassicReason::Rootless);
        node.create(lab.running(id.clone(), &Lab::act(&Act::Wait)))
            .expect("create a box on the rootless node");
        assert_eq!(
            node.classic_creates_for(ClassicReason::Rootless),
            before + 1,
            "a rootless node's create is counted under Rootless"
        );
        assert_eq!(
            node.creates_through_shim(),
            0,
            "a rootless node's create is made by a seed, not through the shim"
        );
        assert_eq!(
            node.pruned_creates(),
            0,
            "a rootless node's seeds don't prune"
        );
        node.delete(&id).expect("delete the rootless box");
        eprintln!(
            "classic reasons: a rootless node's create counted under Rootless, made by a seed, none pruned"
        );
        return;
    }
    // A mount namespace to join by path: a running box's init has one of its own.
    let host = BoxId::new("reason-host").unwrap();
    node.create(lab.running(host.clone(), &Lab::act(&Act::Wait)))
        .expect("create the box whose mount namespace is joined");
    node.start(&host).expect("start it");
    let host_mnt = format!("/proc/{}/ns/mnt", init_pid(&node.container_dir(&host)));
    let base = r#"{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"}"#;
    let userns = r#"{"type":"user"},{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"}"#;
    let mappings = r#","uidMappings":[{"containerID":0,"hostID":0,"size":1}],"gidMappings":[{"containerID":0,"hostID":0,"size":1}]"#;
    let by_path = format!(
        r#"{{"type":"pid"}},{{"type":"mount","path":"{host_mnt}"}},{{"type":"ipc"}},{{"type":"uts"}}"#
    );
    let hooks = r#","hooks":{"createContainer":[{"path":"/usr/bin/true","args":["true"]}]}"#;
    let cases: [(&str, ClassicReason, &str, &str, &str); 3] = [
        ("userns", ClassicReason::UserNamespace, userns, mappings, ""),
        ("hooks", ClassicReason::Hooks, base, "", hooks),
        (
            "mount-by-path",
            ClassicReason::JoinsByPath,
            &by_path,
            "",
            "",
        ),
    ];
    for (name, reason, namespaces, mappings, hooks) in cases {
        let (shim_before, reason_before, pruned_before) = (
            node.creates_through_shim(),
            node.classic_creates_for(reason),
            node.pruned_creates(),
        );
        let dir = tempfile::Builder::new()
            .prefix("bundle-")
            .tempdir_in(lab.bundles.path())
            .unwrap()
            .keep();
        std::fs::write(
            dir.join("config.json"),
            lab.config_ex(&Lab::act(&Act::Wait), namespaces, mappings, "", hooks, ""),
        )
        .unwrap();
        let id = BoxId::new(&format!("reason-{name}")).unwrap();
        // The create routes classic at create (the counter bumps there) whether or not the classic path
        // then completes; delete cleans it up if it did.
        let _ = node.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(dir),
            owner: None,
            band: 0,
        });
        let _ = node.delete(&id);
        assert_eq!(
            node.classic_creates_for(reason),
            reason_before + 1,
            "{name} is counted under {reason:?}"
        );
        assert_eq!(
            node.creates_through_shim(),
            shim_before + 1,
            "{name} took the classic path"
        );
        assert_eq!(
            node.pruned_creates(),
            pruned_before,
            "{name} did not take the pruned path"
        );
    }
    assert_eq!(
        node.classic_creates_for(ClassicReason::Rootless),
        0,
        "a root node counts no create under Rootless"
    );
    node.delete(&host).expect("delete the host box");
    eprintln!(
        "classic reasons: a user namespace (UserNamespace), createContainer hooks (Hooks) and a mount namespace by path (JoinsByPath) each counted, none pruned, none under Rootless"
    );
}

/// OCI hooks (AC-T7.1.4, T-T7.1.4; CLAUDE.md §3; docs/design/runtime.md, "createContainer and
/// startContainer hooks run in the container"; OCI runtime-spec v1.3.0 config.md:607-634, runtime.md:26-27
/// and :62-73). Do: create a box whose bundle names a createRuntime, a createContainer and a startContainer
/// hook, each writing what it sees to a directory the box binds, then start it. Expect: after create, the
/// createRuntime hook saw `created` (runtime.md:20: after lifecycle step 2) and the init's host pid; the
/// createContainer hook ran after it and before the root switch (the bundle's host path in view), in
/// every one of the init's namespaces, given `created` and the pid the container sees (1: the spec makes
/// a pid namespace); the startContainer hook
/// has not run. After start it has, in the init's namespaces inside the root (the host path out of view),
/// given `created` and pid 1, before the program, which finds its record. Neither holds a descriptor but
/// its stdio. A box with a startContainer hook alone takes the classic path, counted as the create-time
/// hooks are (`Hooks` as root, `Rootless` rootless), and its hook runs.
fn create_and_start_container_hooks_run_in_the_containers_namespaces(lab: &Lab, own: &Path) {
    const KINDS: [&str; 7] = ["mnt", "pid", "ipc", "uts", "net", "cgroup", "user"];
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-hooks"), state.path());
    std::fs::create_dir_all(lab.rootfs.join("obs")).unwrap();
    let (namespaces, mappings) = if lab.rootless {
        (
            r#"{"type":"user"},{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"}"#,
            format!(
                r#","uidMappings":[{{"containerID":0,"hostID":{},"size":1}}],"gidMappings":[{{"containerID":0,"hostID":{},"size":1}}]"#,
                lab.uid, lab.gid
            ),
        )
    } else {
        (
            r#"{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"}"#,
            String::new(),
        )
    };
    let classic = if lab.rootless {
        ClassicReason::Rootless
    } else {
        ClassicReason::Hooks
    };
    let sh = |script: String| serde_json::json!({"path": "/bin/sh", "args": ["sh", "-c", script], "env": ["PATH=/usr/bin:/bin"]});
    // A fresh bundle directory and the directory its box binds at /obs.
    let fresh = || {
        let dir = tempfile::Builder::new()
            .prefix("bundle-")
            .tempdir_in(lab.bundles.path())
            .unwrap()
            .keep();
        let obs = dir.join("obs");
        std::fs::create_dir(&obs).unwrap();
        (dir, obs)
    };
    // What a container-namespace hook writes under `at` (/obs as it sees it): its state, its namespaces,
    // the descriptors its `ls` holds (`/proc/self` resolves in the host's procfs before the switch, where
    // the shell's `$$`, its pid in the container's pid namespace, names another process), and whether the
    // bundle's host path is in view.
    let sees = |at: &str, tag: &str, dir: &Path| {
        format!(
            "cat > {at}/{tag}.state; for n in {kinds}; do echo $n $(readlink /proc/self/ns/$n); done > {at}/{tag}.ns; \
             ls -l /proc/self/fd/ > {at}/{tag}.fds; if test -e {bundle}; then echo host; else echo container; fi > {at}/{tag}.root",
            kinds = KINDS.join(" "),
            bundle = dir.join("config.json").display(),
        )
    };
    // Writes the bundle's config with `hooks`; its program records whether the startContainer hook ran
    // before it, then waits.
    let write = |dir: &Path, obs: &Path, hooks: serde_json::Value| {
        let bind = format!(
            r#",
    {{"destination": "/obs", "type": "bind", "source": "{}", "options": ["rbind","rw"]}}"#,
            obs.display()
        );
        let program = strings(&[
            "/bin/sh",
            "-c",
            "if test -e /obs/sc.state; then echo after; else echo before; fi > /obs/program; exec /usr/bin/sleep 600",
        ]);
        let hooks = format!(r#","hooks":{hooks}"#);
        std::fs::write(
            dir.join("config.json"),
            lab.config_ex(&program, namespaces, &mappings, &bind, &hooks, ""),
        )
        .unwrap();
    };
    let make = |id: &BoxId, dir: PathBuf| {
        node.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(dir),
            owner: None,
            band: 0,
        })
    };
    let read =
        |p: PathBuf| std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
    let json = |p: PathBuf| -> serde_json::Value { serde_json::from_str(&read(p)).unwrap() };
    let ns_of = |pid: i64| -> Vec<String> {
        KINDS
            .iter()
            .map(|n| {
                let link = std::fs::read_link(format!("/proc/{pid}/ns/{n}")).unwrap();
                format!("{n} {}", link.display())
            })
            .collect()
    };
    // The hook's `ls` holds what the hook did: its stdio, and besides only the directory it lists.
    let only_stdio = |fds: String, what: &str| {
        let held: Vec<(&str, &str)> = fds
            .lines()
            .filter_map(|l| l.split_once(" -> "))
            .filter_map(|(head, target)| Some((head.rsplit(' ').next()?, target)))
            .collect();
        let numbers: BTreeSet<&str> = held.iter().map(|(fd, _)| *fd).collect();
        assert!(
            ["0", "1", "2"].iter().all(|fd| numbers.contains(fd)),
            "the {what} hook has its stdio: {fds}"
        );
        for (fd, target) in held {
            assert!(
                ["0", "1", "2"].contains(&fd)
                    || (target.starts_with("/proc/") && target.ends_with("/fd")),
                "the {what} hook holds {fd} -> {target}, which it didn't ask for: {fds}"
            );
        }
    };

    // Every create- and start-time hook at once.
    let (dir, obs) = fresh();
    let o = obs.display().to_string();
    write(
        &dir,
        &obs,
        serde_json::json!({
            "createRuntime": [sh(format!("cat > {o}/rt.state"))],
            "createContainer": [sh(format!(
                "if test -e {o}/rt.state; then echo after-createRuntime > {o}/cc.order; fi; {}",
                sees(&o, "cc", &dir)
            ))],
            "startContainer": [sh(sees("/obs", "sc", &dir))],
        }),
    );
    let id = BoxId::new("hooked").unwrap();
    let before = node.classic_creates_for(classic);
    make(&id, dir).expect("create a box with createContainer and startContainer hooks");
    assert_eq!(
        node.classic_creates_for(classic),
        before + 1,
        "it took the classic path, under {classic:?}"
    );
    let container = node.container_dir(&id);
    let init = i64::from(init_pid(&container));
    let rt = json(obs.join("rt.state"));
    assert_eq!(
        (rt["status"].as_str(), rt["pid"].as_i64()),
        (Some("created"), Some(init)),
        "createRuntime's state: {rt}"
    );
    assert_eq!(
        read(obs.join("cc.order")).trim(),
        "after-createRuntime",
        "createContainer ran after createRuntime"
    );
    assert_eq!(
        read(obs.join("cc.root")).trim(),
        "host",
        "createContainer ran before the root switch"
    );
    let cc = json(obs.join("cc.state"));
    assert_eq!(
        (cc["status"].as_str(), cc["pid"].as_i64(), cc["id"].as_str()),
        (
            Some("created"),
            Some(1),
            container.file_name().and_then(|n| n.to_str())
        ),
        "createContainer's state: {cc}"
    );
    let init_ns = ns_of(init);
    assert_eq!(
        read(obs.join("cc.ns")).lines().collect::<Vec<_>>(),
        init_ns,
        "createContainer ran in the init's namespaces"
    );
    let ours = ns_of(i64::from(std::process::id()));
    for (at, kind) in KINDS.iter().enumerate().take(4) {
        assert_ne!(
            init_ns[at], ours[at],
            "the container's {kind} namespace is its own, not the agent's"
        );
    }
    only_stdio(read(obs.join("cc.fds")), "createContainer");
    assert!(
        !obs.join("sc.state").exists(),
        "startContainer waits for start"
    );
    // The init waits for start with SIGPIPE ignored (a `hex` process, as Rust's runtime leaves it), so a
    // start gone before it hands the byte is an EPIPE it ends on by its own status, never the signal
    // (hex_runtime's hooks::START_ABANDONED).
    let status = read(PathBuf::from(format!("/proc/{init}/status")));
    let ignored = status
        .lines()
        .find_map(|l| l.strip_prefix("SigIgn:"))
        .map(|m| u64::from_str_radix(m.trim(), 16).unwrap())
        .unwrap();
    const SIGPIPE: u32 = 13; // signal(7), on every Linux architecture Hex runs on
    assert_ne!(
        ignored & (1 << (SIGPIPE - 1)),
        0,
        "the init ignores SIGPIPE: SigIgn {ignored:#x}"
    );

    node.start(&id).expect("start the hooked box");
    // The program's shell makes its file as it opens it for the line, before it writes the line: the fact is the line.
    wait_for(
        &PROGRAM_RUN,
        || std::fs::read_to_string(obs.join("program")).is_ok_and(|l| l.ends_with('\n')),
        "the program to write its line",
    );
    assert_eq!(
        read(obs.join("program")).trim(),
        "after",
        "startContainer ran before the program"
    );
    assert_eq!(
        read(obs.join("sc.root")).trim(),
        "container",
        "startContainer ran inside the root"
    );
    let sc = json(obs.join("sc.state"));
    assert_eq!(
        (sc["status"].as_str(), sc["pid"].as_i64()),
        (Some("created"), Some(1)),
        "startContainer's state: {sc}"
    );
    assert_eq!(
        read(obs.join("sc.ns")).lines().collect::<Vec<_>>(),
        init_ns,
        "startContainer ran in the init's namespaces"
    );
    only_stdio(read(obs.join("sc.fds")), "startContainer");
    node.delete(&id).expect("delete the hooked box");

    // A startContainer hook alone routes the create classic as well, and runs.
    let (dir, obs) = fresh();
    write(
        &dir,
        &obs,
        serde_json::json!({"startContainer": [sh("echo ran > /obs/sc.state".to_owned())]}),
    );
    let id = BoxId::new("hooked-start").unwrap();
    let before = node.classic_creates_for(classic);
    make(&id, dir).expect("create a box with a startContainer hook alone");
    assert_eq!(
        node.classic_creates_for(classic),
        before + 1,
        "a startContainer hook takes the classic path"
    );
    node.start(&id).expect("start it");
    // The program's shell makes its file as it opens it for the line, before it writes the line: the fact is the line.
    wait_for(
        &PROGRAM_RUN,
        || std::fs::read_to_string(obs.join("program")).is_ok_and(|l| l.ends_with('\n')),
        "the program to write its line",
    );
    assert_eq!(
        read(obs.join("program")).trim(),
        "after",
        "its startContainer hook ran first"
    );
    node.delete(&id).expect("delete it");

    eprintln!(
        "hooks: createRuntime, createContainer and startContainer each ran at its point and in its namespaces"
    );
}

/// A bundle whose box binds a fresh directory at /obs, runs a program that writes /obs/program, and names
/// `hooks`: the failing-hook tests' (AC-T7.1.4).
fn hooked_bundle(lab: &Lab, hooks: serde_json::Value) -> (PathBuf, PathBuf) {
    let dir = tempfile::Builder::new()
        .prefix("bundle-")
        .tempdir_in(lab.bundles.path())
        .unwrap()
        .keep();
    let obs = dir.join("obs");
    std::fs::create_dir(&obs).unwrap();
    std::fs::create_dir_all(lab.rootfs.join("obs")).unwrap();
    let program = strings(&[
        "/bin/sh",
        "-c",
        "echo ran > /obs/program; exec /usr/bin/sleep 600",
    ]);
    let mut spec: serde_json::Value = serde_json::from_str(&lab.config(&program, false)).unwrap();
    spec["mounts"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!(
            {"destination": "/obs", "type": "bind", "source": obs, "options": ["rbind", "rw"]}
        ));
    spec["hooks"] = hooks;
    std::fs::write(dir.join("config.json"), spec.to_string()).unwrap();
    (dir, obs)
}

/// A hook that exits 3, run by `/bin/sh` (found in the runtime's namespace and in the box's root alike).
fn failing_hook() -> serde_json::Value {
    serde_json::json!([{"path": "/bin/sh", "args": ["sh", "-c", "exit 3"], "env": ["PATH=/usr/bin:/bin"]}])
}

/// runtime.md:66-67 (AC-T7.1.4, T-T7.1.4; CLAUDE.md §3). Do: create a box whose createContainer hook exits 3.
/// Expect: the create refused by type, an `Os` refusal naming the createContainer hook, never the refusal a
/// stopped box gets (`NotRunning`); the box isn't there afterwards and its program never ran.
fn a_failing_create_container_hook_fails_the_create_naming_it(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-hook-create-fails"), state.path());
    let (dir, obs) = hooked_bundle(lab, serde_json::json!({"createContainer": failing_hook()}));
    let id = BoxId::new("hooked-create-fails").unwrap();
    let refused = node
        .create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(dir),
            owner: None,
            band: 0,
        })
        .expect_err("a failing createContainer hook fails the create");
    assert!(
        matches!(&refused, Refusal::Os { why, .. } if why.contains("createContainer hook failed") && !why.contains("stopped")),
        "the refusal is typed and names the hook: {refused:?}"
    );
    assert_eq!(
        node.inspect(&id),
        Err(Refusal::NoSuchBox(id.clone())),
        "a refused create leaves no box"
    );
    assert!(!obs.join("program").exists(), "the program never ran");
    eprintln!("hooks: a failing createContainer hook failed the create, naming it: {refused:?}");
}

/// runtime.md:69-70 (AC-T7.1.4, T-T7.1.4; CLAUDE.md §3). Do: create a box whose startContainer hook exits 3,
/// then start it. Expect: the create made, the start refused by type, an `Os` refusal naming the
/// startContainer hook, never `NotRunning` (the refusal a stopped box's start gets); its program never ran.
fn a_failing_start_container_hook_fails_the_start_naming_it(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-hook-start-fails"), state.path());
    let (dir, obs) = hooked_bundle(lab, serde_json::json!({"startContainer": failing_hook()}));
    let id = BoxId::new("hooked-start-fails").unwrap();
    node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Bundle(dir),
        owner: None,
        band: 0,
    })
    .expect("create a box whose startContainer hook fails");
    let refused = node
        .start(&id)
        .expect_err("a failing startContainer hook fails the start");
    assert!(
        matches!(&refused, Refusal::Os { why, .. } if why.contains("startContainer hook failed") && !why.contains("stopped")),
        "the refusal is typed and names the hook: {refused:?}"
    );
    assert!(!obs.join("program").exists(), "the program never ran");
    node.delete(&id).expect("delete the box");
    eprintln!("hooks: a failing startContainer hook failed the start, naming it: {refused:?}");
}

/// Goal 5 plan step 2 (T-T7.1.4): rootfsPropagation shared, slave and private each show on the box's
/// root in its own mountinfo, and no mount the pruned init makes reaches the agent's mount namespace.
/// Each box binds a uniquely named volume; were the init's bind to propagate back to the agent (a shared
/// root sharing a peer group with the agent's mounts), that volume's source would be a mount in the
/// agent's mountinfo. Root only.
fn rootfs_propagation_shows_in_the_box_not_the_agent(lab: &Lab, own: &Path) {
    if lab.rootless {
        return;
    }
    // v6.19 fs/namespace.c:1259-1288: clone_mnt keeps the source's peer/master unless asked to
    // change it. open_tree's detached copy calls __do_loopback (:3032, :2963-2966). A private
    // source cannot acquire a master by MS_SLAVE: fs/pnode.c:93-117 and mount_namespaces(7),
    // "Propagation type transitions". The lab must establish its own shared inputs.
    struct SharedSource(PathBuf);
    impl SharedSource {
        fn make(path: &Path) -> Self {
            rustix::mount::mount_bind(path, path).unwrap_or_else(|e| {
                panic!("binding a propagation-test source {}: {e}", path.display())
            });
            let source = Self(path.to_owned());
            rustix::mount::mount_change(path, rustix::mount::MountPropagationFlags::PRIVATE)
                .unwrap_or_else(|e| panic!("isolating a propagation-test source: {e}"));
            rustix::mount::mount_change(path, rustix::mount::MountPropagationFlags::SHARED)
                .unwrap_or_else(|e| panic!("sharing a propagation-test source: {e}"));
            source
        }
        fn peer(&self) -> String {
            let text = std::fs::read_to_string("/proc/self/mountinfo").unwrap();
            let mounts = mountinfo(&text);
            let source = mounts
                .iter()
                .rev()
                .find(|(point, ..)| Path::new(point) == self.0)
                .unwrap_or_else(|| panic!("source is not a mount: {}", self.0.display()));
            source
                .2
                .split_whitespace()
                .find_map(|tag| tag.strip_prefix("shared:"))
                .unwrap_or_else(|| panic!("source must be shared before create: {source:?}"))
                .to_owned()
        }
    }
    impl Drop for SharedSource {
        fn drop(&mut self) {
            let private = rustix::mount::mount_change(
                &self.0,
                rustix::mount::MountPropagationFlags::PRIVATE
                    | rustix::mount::MountPropagationFlags::REC,
            );
            let detached = rustix::mount::unmount(&self.0, rustix::mount::UnmountFlags::DETACH);
            if private.is_err() || detached.is_err() {
                if std::thread::panicking() {
                    eprintln!("shared-source cleanup: private={private:?}, detach={detached:?}");
                } else {
                    panic!("shared-source cleanup: private={private:?}, detach={detached:?}");
                }
            }
        }
    }
    struct Created<'a> {
        node: &'a Node,
        id: BoxId,
        live: bool,
    }
    impl Created<'_> {
        fn delete(&mut self) {
            self.node
                .delete(&self.id)
                .expect("delete the propagation-test box");
            self.live = false;
        }
    }
    impl Drop for Created<'_> {
        fn drop(&mut self) {
            if self.live
                && let Err(e) = self.node.delete(&self.id)
            {
                if std::thread::panicking() {
                    eprintln!("propagation-test box cleanup: {e}");
                } else {
                    panic!("propagation-test box cleanup: {e}");
                }
            }
        }
    }
    let table = || {
        std::fs::read_to_string("/proc/self/mountinfo")
            .unwrap()
            .lines()
            .map(str::to_owned)
            .collect::<BTreeSet<_>>()
    };
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-prop"), state.path());
    for prop in ["plain", "shared", "slave", "private"] {
        let case = tempfile::Builder::new()
            .prefix("shared-sources-")
            .tempdir_in(lab.bundles.path())
            .unwrap();
        let rootfs = case.path().join("rootfs");
        build_rootfs(&rootfs);
        let bound = case.path().join("bind-source");
        std::fs::create_dir_all(bound.join("child")).unwrap();
        std::fs::write(bound.join("masked"), "source file").unwrap();
        std::fs::write(bound.join("readonly"), "source file").unwrap();
        let root_source = SharedSource::make(&rootfs);
        let bind_source = SharedSource::make(&bound);
        let root_peer = root_source.peer();
        let bind_peer = bind_source.peer();
        eprintln!("{prop}: root source shared:{root_peer}, bind source shared:{bind_peer}");
        let before = table();
        let mut config: serde_json::Value =
            serde_json::from_str(&lab.config(&Lab::act(&Act::Wait), false)).unwrap();
        config["root"]["path"] = serde_json::json!(rootfs);
        if prop != "plain" {
            config["linux"]["rootfsPropagation"] = serde_json::json!(prop);
        }
        config["linux"]["maskedPaths"] = serde_json::json!(["/proc/kcore", "/shared-bind/masked"]);
        config["linux"]["readonlyPaths"] =
            serde_json::json!(["/proc/sys", "/shared-bind/readonly"]);
        config["mounts"].as_array_mut().unwrap().extend([
            serde_json::json!({"destination":"/dev/pts", "type":"devpts", "source":"devpts",
                "options":["nosuid","noexec","newinstance","ptmxmode=0666","mode=0620"]}),
            serde_json::json!({"destination":"/dev/shm", "type":"tmpfs", "source":"shm",
                "options":["nosuid","nodev","mode=1777","size=64k"]}),
            serde_json::json!({"destination":"/dev/mqueue", "type":"mqueue", "source":"mqueue"}),
            serde_json::json!({"destination":"/shared-bind", "type":"bind", "source":bound,
                "options":["rbind","rw"]}),
            serde_json::json!({"destination":"/shared-bind/child", "type":"tmpfs", "source":"bind-child",
                "options":["mode=700","size=64k"]}),
        ]);
        std::fs::write(
            case.path().join("config.json"),
            serde_json::to_vec(&config).unwrap(),
        )
        .unwrap();
        let id = BoxId::new(&format!("prop-{prop}")).unwrap();
        let pruned = node.pruned_creates();
        node.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(case.path().to_owned()),
            owner: None,
            band: 0,
        })
        .unwrap_or_else(|e| panic!("create the {prop} shared-source box: {e:?}"));
        let mut made = Created {
            node: &node,
            id: id.clone(),
            live: true,
        };
        assert_eq!(node.pruned_creates(), pruned + 1, "{prop} must run pruned");
        let during = table();
        let gained: Vec<_> = during.difference(&before).collect();
        assert_eq!(
            during, before,
            "{prop}: agent gained mounts during create: {gained:?}"
        );
        node.start(&id).expect("start the propagation-test box");
        let (exit, mi) = run_in(&node, &id, &["/usr/bin/cat", "/proc/self/mountinfo"]);
        assert_eq!(exit, Exit::Code(0), "read the {prop} box's mountinfo: {mi}");
        let mounts = mountinfo(&mi);
        let root = mounts
            .iter()
            .rev()
            .find(|(point, ..)| point == "/")
            .unwrap_or_else(|| panic!("{prop}: box has no root mount: {mi}"));
        let tags: Vec<_> = root.2.split_whitespace().collect();
        match prop {
            "slave" => {
                assert!(
                    tags.contains(&format!("master:{root_peer}").as_str()),
                    "{prop}: exact root master:{root_peer} required: {root:?}"
                );
                assert!(
                    !tags.iter().any(|tag| tag.starts_with("shared:")),
                    "{root:?}"
                );
            }
            "shared" => {
                let peer = tags
                    .iter()
                    .find_map(|tag| tag.strip_prefix("shared:"))
                    .unwrap_or_else(|| panic!("shared root has no peer group: {root:?}"));
                assert_ne!(
                    peer, root_peer,
                    "box must not share its source's peer group"
                );
            }
            _ => assert!(
                !tags
                    .iter()
                    .any(|tag| tag.starts_with("shared:") || tag.starts_with("master:")),
                "{prop}: root must be private: {root:?}"
            ),
        }
        let bound_mount = mounts
            .iter()
            .find(|(point, ..)| point == "/shared-bind")
            .unwrap();
        assert!(
            !bound_mount
                .2
                .split_whitespace()
                .any(|tag| tag.starts_with("shared:") || tag.starts_with("master:")),
            "the default bind must be private: {bound_mount:?}"
        );
        assert_eq!(table(), before, "{prop}: agent changed while box ran");
        made.delete();
        let after = table();
        let gained: Vec<_> = after.difference(&before).collect();
        assert_eq!(
            after, before,
            "{prop}: agent gained mounts after delete: {gained:?}"
        );
    }
    eprintln!(
        "rootfsPropagation plain/shared/slave/private: shared root and bind inputs checked; agent table unchanged before/during/after each pruned box"
    );
}

/// Goal 5 plan step 2 (T-T7.1.4; CLAUDE.md §3, §6): the pruned init refusing or dying mid-build leaves
/// nothing of the container in the agent's own namespace. Two ways in, both after the rootfs is attached
/// and before the root switch: (1) a filesystem mount of a type the kernel doesn't have — the init
/// refuses at `fsopen`, by type, naming the mount and why; (2) the init killed between its mounts' attach
/// and the `chroot` (a debug-only crash point armed on the seed) — refused by type, its mounts
/// dying with its own namespace. Each time: the box is gone, its directory gone, the agent's mounts
/// unchanged. Root only: a pruned seed is a root node's, and the pruned path is what keeps the
/// container's mounts out of the agent's namespace.
fn an_init_that_refuses_or_dies_mid_build_leaves_nothing(lab: &Lab, own: &Path) {
    if lab.rootless {
        return;
    }

    // (1) An unattachable filesystem mount: the pruned init reaches it after attaching the rootfs and
    // refuses at `fsopen`, by type, naming the mount and why; the agent then leaves nothing and serves on.
    {
        let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
        let box_root = own.join("hex-oci-attach-fail");
        let node = lab.node(&box_root, state.path());
        let bad =
            ",\n    {\"destination\": \"/bad\", \"type\": \"nosuchfs\", \"source\": \"nosuchfs\"}";
        let dir = tempfile::Builder::new()
            .prefix("bundle-")
            .tempdir_in(lab.bundles.path())
            .unwrap()
            .keep();
        std::fs::write(
            dir.join("config.json"),
            lab.config_with(
                &Lab::act(&Act::Wait),
                r#"{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"}"#,
                "",
                bad,
            ),
        )
        .unwrap();
        let id = BoxId::new("attach-fail").unwrap();
        let before = agent_mount_points();
        let refused = node.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(dir.clone()),
            owner: None,
            band: 0,
        });
        assert!(
            matches!(&refused, Err(Refusal::Os { why, .. }) if why.contains("nosuchfs") && why.contains("/bad")),
            "the unattachable mount is refused by type, naming the mount and why: {refused:?}"
        );
        assert_eq!(
            node.creates_through_shim(),
            0,
            "the create took the pruned path, not a classic fallback"
        );
        assert_eq!(
            node.inspect(&id),
            Err(Refusal::NoSuchBox(id.clone())),
            "the refused box is cleaned up"
        );
        assert!(
            !box_root.join(OCI_BOXES).join(id.to_string()).exists(),
            "the refused box's directory is gone"
        );
        assert_eq!(
            agent_mount_points(),
            before,
            "no mount of the refused container is in the agent's namespace"
        );
        // The node serves a clean create after the refusal: the pruned path isn't wedged.
        let ok = BoxId::new("attach-fail-then-ok").unwrap();
        node.create(lab.running(ok.clone(), &Lab::act(&Act::Wait)))
            .expect("the next create succeeds");
        node.start(&ok).expect("start the next box");
        assert_eq!(node.inspect(&ok).unwrap().state, BoxState::Running);
        node.delete(&ok).expect("delete");
    }

    // (2) The init killed between the mounts' attach and the root switch: debug builds only, where the
    // crash point exists. Every seed of this node is armed, so it serves no clean create — the refusal
    // path's recovery is shown in (1).
    #[cfg(debug_assertions)]
    {
        let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
        let box_root = own.join("hex-oci-killed-build");
        let node = Node::open(&box_root, state.path(), state.path())
            .expect("open a node")
            .with_inject(hex_runtime::linux::oci::seed::INJECT_DIE_BEFORE_SWITCH)
            .with_oci_helper(PathBuf::from(&lab.exe));
        let id = BoxId::new("killed-build").unwrap();
        let before = agent_mount_points();
        let refused = node.create(lab.running(id.clone(), &Lab::act(&Act::Wait)));
        assert!(
            matches!(&refused, Err(Refusal::Os { .. }) | Err(Refusal::Broken(_))),
            "the init killed mid-build refuses the create by type: {refused:?}"
        );
        assert_eq!(
            node.creates_through_shim(),
            0,
            "the killed create took the pruned path, not a classic fallback"
        );
        assert_eq!(
            node.inspect(&id),
            Err(Refusal::NoSuchBox(id.clone())),
            "the killed box is cleaned up"
        );
        assert!(
            !box_root.join(OCI_BOXES).join(id.to_string()).exists(),
            "the killed box's directory is gone"
        );
        assert_eq!(
            agent_mount_points(),
            before,
            "no mount of the killed container is in the agent's namespace"
        );
    }

    // (3) Only the node's side can arm the crash point, never a container: a box on a node that armed
    // nothing, whose own spec sets the inject variable in its process env, builds its root and runs — the
    // init never saw it (the env reaches the workload at execve, after the root is built, and the node
    // clears it from the seed besides, so no box can kill its own init through it). Debug builds only,
    // where the crash point exists to be proven inert.
    #[cfg(debug_assertions)]
    {
        let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
        let box_root = own.join("hex-oci-spec-inject");
        let node = lab.node(&box_root, state.path());
        let inject = hex_runtime::linux::oci::seed::ENV_INJECT;
        let point = hex_runtime::linux::oci::seed::INJECT_DIE_BEFORE_SWITCH;
        let dir = tempfile::Builder::new()
            .prefix("bundle-")
            .tempdir_in(lab.bundles.path())
            .unwrap()
            .keep();
        let config = lab
            .config_with(
                &Lab::act(&Act::Wait),
                r#"{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"}"#,
                "",
                "",
            )
            .replace(
                r#""env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"]"#,
                &format!(
                    r#""env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin", "{inject}={point}"]"#
                ),
            );
        assert!(
            config.contains(&format!("{inject}={point}")),
            "the box's spec carries the inject variable in its process env"
        );
        std::fs::write(dir.join("config.json"), config).unwrap();
        let id = BoxId::new("spec-inject").unwrap();
        node.create(BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(dir),
            owner: None,
            band: 0,
        })
        .expect("the box whose spec sets the inject variable is created: the init never saw it");
        node.start(&id).expect("start");
        assert_eq!(
            node.inspect(&id).unwrap().state,
            BoxState::Running,
            "the box runs: its own env did not arm the crash point, only the node's side can"
        );
        node.delete(&id).expect("delete");
    }

    eprintln!(
        "mid-build: an unattachable mount is refused naming it, a killed init refused, and a container's own env cannot arm the crash point; each leaves no box, directory or mount in the agent's namespace"
    );
}

/// HEX-RT-14, the bound learned (CLAUDE.md §3): the node keeps its RLIMIT_STACK from its first reading, so
/// a limit lowered after it is first met where a program starts: as root, the agent's vfork-style child's
/// `execve` fails `E2BIG`; rootless, the seed (started under the lower limit) refuses the program by its
/// own reading. Do: lower this node's RLIMIT_STACK after its first reading to 512 KiB (whose bound is
/// Linux's floor, `ARG_MAX`), then exec a program between the two limits' bounds and within the seed
/// wire's one message, twice. Expect: the first is refused as `Invalid` "program" and the node reads its
/// limit again, once; the second is refused the same way by the node itself, with no further reading; a
/// small program still runs. Rootless, a program within the node's bound but past the seed wire's one
/// message is refused as `Invalid` "program" too, with no reading: no seed can be handed it.
fn a_program_past_the_kernels_bound_is_refused_by_name_and_learned(lab: &Lab, own: &Path) {
    use hex_runtime::linux::process::{exec_limit, exec_limit_for, relearn_stack, stack_reads};
    use rustix::process::{Resource, Rlimit, getrlimit, setrlimit};
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-e2big"), state.path());
    let id = BoxId::new("e2big").unwrap();
    node.create(lab.spec(id.clone(), Bounds::default()))
        .expect("create");
    node.start(&id).expect("start");
    let high = exec_limit().block;
    let was = getrlimit(Resource::Stack);
    let lower = 512 << 10;
    let low = exec_limit_for(lower).block;
    let wire = hex_core::seed::MAX_REQUEST;
    assert!(
        low < wire && wire < high,
        "this host's stack bound ({high}) is above the seed wire's message ({wire}), above a 512 KiB stack's ({low})"
    );
    // Padded a page at a time past the lower bound, by an eighth of the room the seed's message leaves.
    let padded = |past: usize| {
        let mut program = lab.program(&Act::Say("past"));
        let mut n = 0;
        while program.exec_size() <= past {
            program.env.push((format!("PAD{n}"), "x".repeat(4 << 10)));
            n += 1;
        }
        program
    };
    let program = padded(low + (wire - low) / 8);
    setrlimit(
        Resource::Stack,
        Rlimit {
            current: Some(lower),
            maximum: was.maximum,
        },
    )
    .expect("lower this node's stack limit");
    let reads = stack_reads();
    let first = node.exec(&id, program.clone(), hex_api::Stdio::null());
    let after = stack_reads();
    let second = node.exec(&id, program, hex_api::Stdio::null());
    let small = node
        .exec(&id, lab.program(&Act::Say("small")), hex_api::Stdio::null())
        .map(|mut p| p.wait_for(conformance::patience()));
    let last = stack_reads();
    setrlimit(Resource::Stack, was).expect("restore this node's stack limit");
    relearn_stack();
    // After the stack case, whose seed must start under the lower limit: asking for one starts it.
    if lab.rootless {
        // Two pages past, since the wire counts each string's length where execve counts a pointer and a NUL.
        let reads = stack_reads();
        let oversized = node.exec(&id, padded(wire + (8 << 10)), hex_api::Stdio::null());
        assert!(
            matches!(
                oversized,
                Err(Refusal::Invalid {
                    what: "program",
                    ..
                })
            ),
            "a program past the seed wire's one message is refused by name, got {:?}",
            oversized.map(|p| p.pid())
        );
        assert_eq!(
            stack_reads(),
            reads,
            "the wire's bound reads no stack limit"
        );
    }
    assert!(
        matches!(
            first,
            Err(Refusal::Invalid {
                what: "program",
                ..
            })
        ),
        "the first exec past the kernel's bound is refused by name, got {:?}",
        first.map(|p| p.pid())
    );
    assert_eq!(
        after - reads,
        1,
        "the refusal read the node's limit again, once"
    );
    assert!(
        matches!(
            second,
            Err(Refusal::Invalid {
                what: "program",
                ..
            })
        ),
        "the second is refused by the node's learned bound, got {:?}",
        second.map(|p| p.pid())
    );
    assert_eq!(
        last, after,
        "the learned bound refused it with no further reading"
    );
    assert!(
        matches!(small, Ok(Ok(Some(Exit::Code(0))))),
        "a program within the bound still runs, got {small:?}"
    );
    node.delete(&id).expect("delete");
}

/// Goal 5 plan step 4: the box bound's count is the root's link count, not a listing (a listing waits
/// behind every box's `mkdir` and `rmdir` beside it). Do: make three boxes under a box root, then remove
/// one. Expect: the count equals the listing's at each step — 0, 3, then 2.
fn a_box_root_counts_its_boxes_without_listing(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let root = BoxRoot::open(
        &own.join("hex-oci-count"),
        disks::under(state.path()).unwrap(),
    )
    .unwrap();
    assert_eq!((root.count().unwrap(), root.boxes().unwrap().len()), (0, 0));
    let ids: Vec<BoxId> = ["c1", "c2", "c3"]
        .iter()
        .map(|n| BoxId::new(n).unwrap())
        .collect();
    let made: Vec<_> = ids
        .iter()
        .map(|id| root.create(id, &Bounds::default()).unwrap())
        .collect();
    assert_eq!((root.count().unwrap(), root.boxes().unwrap().len()), (3, 3));
    made[0].remove().unwrap();
    assert_eq!((root.count().unwrap(), root.boxes().unwrap().len()), (2, 2));
    for cg in &made[1..] {
        cg.remove().unwrap();
    }
}

/// Goal 5 plan step 4 (decision 5, T-T7.1.4): a given bundle that joins a namespace by path is made
/// through the seam, as the shim made it before. Do: run box A with a network namespace of its own;
/// create and start box B whose bundle names A's network namespace by path — and, rootless, A's user
/// namespace too, which a rootless container needs to join a namespace A's user namespace owns. Expect: B
/// runs in A's network namespace (and A's user namespace, rootless). As root, B's init joined A's network
/// namespace itself after the seed's clone, so no create went through the shim; rootless, a user namespace
/// by path can't be joined in one clone, so B was made through the shim and counted once — the one kind of
/// create that still re-execs the hex binary. Either way both inits' streams are the null device, a seed's
/// and a seam shim's alike, never the node agent's.
fn a_bundle_joining_a_namespace_by_path_runs_through_the_seam(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-joinpath"), state.path());
    let host = BoxId::new("join-host").unwrap();
    node.create(lab.networked(host.clone()))
        .expect("create the box whose namespaces are joined");
    node.start(&host).expect("start it");
    let host_init = init_pid(&node.container_dir(&host));
    let path = |ns: &str| format!("/proc/{host_init}/ns/{ns}");
    let rest = r#"{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"}"#;
    let namespaces = if lab.rootless {
        format!(
            r#"{{"type":"user","path":"{}"}},{{"type":"network","path":"{}"}},{rest}"#,
            path("user"),
            path("net")
        )
    } else {
        format!(r#"{{"type":"network","path":"{}"}},{rest}"#, path("net"))
    };
    let dir = tempfile::Builder::new()
        .prefix("bundle-")
        .tempdir_in(lab.bundles.path())
        .unwrap()
        .keep();
    std::fs::write(
        dir.join("config.json"),
        lab.config_with(&Lab::act(&Act::Wait), &namespaces, "", ""),
    )
    .unwrap();
    let joiner = BoxId::new("join-path").unwrap();
    node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: joiner.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Bundle(dir),
        owner: None,
        band: 0,
    })
    .expect("a bundle joining a namespace by path is made through the seam");
    node.start(&joiner).expect("start the joiner");
    let joined = init_pid(&node.container_dir(&joiner));
    assert_eq!(
        netns_of(joined),
        netns_of(host_init),
        "it runs in the network namespace it joined by path"
    );
    for (who, pid) in [("the seed-made box", host_init), ("the joiner", joined)] {
        assert_eq!(
            streams(pid),
            ["/dev/null"; 3].map(PathBuf::from),
            "{who}'s init has the null device for its streams"
        );
    }
    if lab.rootless {
        let userns = |pid: i32| std::fs::read_link(format!("/proc/{pid}/ns/user")).unwrap();
        assert_eq!(
            userns(joined),
            userns(host_init),
            "and the user namespace it joined"
        );
        assert_eq!(
            node.creates_through_shim(),
            1,
            "a user namespace by path is made through the shim, counted"
        );
    } else {
        assert_eq!(
            node.creates_through_shim(),
            0,
            "a network namespace by path is joined by the seed's clone, not the shim"
        );
    }
    node.delete(&joiner).expect("delete the joiner");
    node.delete(&host).expect("delete the box it joined");
}

/// The agent the crash kills: a node in this re-exec'd process makes and starts a box whose init
/// waits, says `ready <agent pid> <its container's state dir>` on stdout, and waits to be killed.
fn play_the_agent(dirs: &Path, exe: &str) -> ! {
    let lab_rootfs = dirs.join("rootfs");
    let box_root = PathBuf::from(std::fs::read_to_string(dirs.join("box_root")).unwrap());
    let state = dirs.join("state");
    let bundle = dirs.join("bundle");
    let node = Node::open(&box_root, &state, &state)
        .expect("agent: open a node")
        .with_oci_helper(PathBuf::from(exe));
    let id = BoxId::new("crash").unwrap();
    assert!(lab_rootfs.exists());
    let made = node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Bundle(bundle),
        owner: None,
        band: 0,
    });
    match made {
        // Waits to be killed either way, as the harness ends it.
        Err(e) => println!("unavailable {e}"),
        Ok(_) => {
            node.start(&id).expect("agent: start");
            println!(
                "ready {} {}",
                std::process::id(),
                node.container_dir(&id).display()
            );
        }
    }
    loop {
        std::thread::park();
    }
}

/// T-T7.1.2 crash injection: SIGKILL the agent while its box runs; a restarted agent, not the
/// init's parent, reads the box running while the init runs and empty once it has ended (an exit it
/// couldn't collect: `Unknown`), and nothing of the box is left. `Err` when no container can be
/// made here.
fn crash_and_restart(lab: &Lab, own: &Path) -> Result<(), String> {
    let dirs = tempfile::Builder::new()
        .prefix("crash-")
        .tempdir_in(lab.bundles.path())
        .unwrap();
    let box_root = own.join("hex-oci-crash");
    let state = dirs.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(
        dirs.path().join("box_root"),
        box_root.to_string_lossy().as_bytes(),
    )
    .unwrap();
    std::os::unix::fs::symlink(&lab.rootfs, dirs.path().join("rootfs")).unwrap();
    let bundle = dirs.path().join("bundle");
    std::fs::create_dir_all(&bundle).unwrap();
    std::fs::write(
        bundle.join("config.json"),
        lab.config(&Lab::act(&Act::Wait), false),
    )
    .unwrap();

    let mut agent = Command::new(std::env::current_exe().unwrap())
        .args([TEST, "--exact", "--nocapture", "--test-threads=1"])
        .env(AGENT_ENV, dirs.path())
        .stdout(Std::piped())
        .spawn()
        .expect("start the agent");
    // libtest writes `test <name> ... ` before the test's own output, on the same line.
    let mut out = BufReader::new(agent.stdout.take().unwrap());
    let (agent_pid, container): (i32, PathBuf) = loop {
        let mut line = String::new();
        assert!(
            out.read_line(&mut line).unwrap() > 0,
            "the agent ended before it was ready"
        );
        if let Some((_, ready)) = line.split_once("ready ") {
            let (pid, dir) = ready.trim().split_once(' ').unwrap();
            break (pid.parse().unwrap(), PathBuf::from(dir));
        }
        if let Some((_, why)) = line.split_once("unavailable ") {
            agent.kill().unwrap();
            agent.wait().unwrap();
            return Err(why.trim().to_owned());
        }
    };
    let init = init_pid(&container);
    assert_eq!(parent_of(init), agent_pid, "the init is the agent's child");

    agent.kill().unwrap();
    agent.wait().unwrap();
    // Orphaned again: its new parent is the agent's nearest subreaper, not this process.
    wait_for(
        &PROGRAM_RUN,
        || parent_of(init) != agent_pid,
        "the init to be reparented",
    );

    let restarted = lab.node(&box_root, &state);
    let id = BoxId::new("crash").unwrap();
    let summary = restarted.recover().expect("recover");
    assert!(summary.adopted.contains(&id), "not adopted: {summary:?}");
    let running = restarted.inspect(&id).unwrap();
    assert_eq!(
        (running.state, running.exit),
        (BoxState::Running, None),
        "while its init runs"
    );

    kill(init, 9);
    let ended = wait_exit(&restarted, &id);
    assert_eq!(
        (ended.state, ended.exit),
        (BoxState::Empty, Some(Exit::Unknown)),
        "once its init has ended"
    );
    wait_for(
        &KILLED,
        || !Path::new(&format!("/proc/{init}")).exists(),
        "the ended init to be reaped by its parent",
    );
    restarted.delete(&id).expect("delete");
    assert!(
        !box_root.join(OCI_BOXES).join("crash").exists(),
        "the box's cgroup is left"
    );
    assert!(restarted.list().unwrap().is_empty(), "a box is left");
    Ok(())
}

/// Decision 2: once create returns, the init is the agent's child; its program's exit 7 is what
/// inspect reports, and the init is reaped (its pid gone).
fn parent_and_exit_code(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-exit"), state.path());
    let id = BoxId::new("exit7").unwrap();
    let info = node
        .create(lab.running(id.clone(), &strings(&["/bin/sh", "-c", "exit 7"])))
        .expect("create");
    assert_eq!((info.state, info.exit), (BoxState::Created, None));
    let init = init_pid(&node.container_dir(&id));
    assert_eq!(
        parent_of(init),
        i32::try_from(std::process::id()).unwrap(),
        "the init's parent is the agent"
    );
    node.start(&id).expect("start");
    let ended = wait_exit(&node, &id);
    assert_eq!(
        (ended.state, ended.exit),
        (BoxState::Empty, Some(Exit::Code(7)))
    );
    assert!(
        !Path::new(&format!("/proc/{init}")).exists(),
        "the ended init is reaped"
    );
    node.delete(&id).expect("delete");
}

/// SIGKILL a running box's init: inspect reports signal 9, and no zombie of it remains.
fn killed_init(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-kill"), state.path());
    let id = BoxId::new("killed").unwrap();
    node.create(lab.running(id.clone(), &Lab::act(&Act::Wait)))
        .expect("create");
    node.start(&id).expect("start");
    let init = init_pid(&node.container_dir(&id));
    kill(init, 9);
    let ended = wait_exit(&node, &id);
    assert_eq!(
        (ended.state, ended.exit),
        (BoxState::Empty, Some(Exit::Signal(9)))
    );
    assert!(
        !Path::new(&format!("/proc/{init}")).exists(),
        "no zombie of it remains"
    );
    node.delete(&id).expect("delete");
}

/// Past the init budget create is refused, typed and naming it, and makes nothing; a delete gives
/// the place back.
fn budget(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let box_root = own.join("hex-oci-budget");
    let node = lab
        .node(&box_root, state.path())
        .with_init_budget(1)
        .unwrap();
    let wait = Lab::act(&Act::Wait);
    let first = BoxId::new("first").unwrap();
    node.create(lab.running(first.clone(), &wait))
        .expect("the first create fits the budget");
    let second = BoxId::new("second").unwrap();
    let refused = node.create(lab.running(second.clone(), &wait));
    assert_eq!(
        refused,
        Err(Refusal::Exhausted {
            what: hex_node::supervise::PIDFDS,
            bound: 1
        })
    );
    assert_eq!(
        node.inspect(&second),
        Err(Refusal::NoSuchBox(second.clone()))
    );
    assert!(
        !box_root.join(OCI_BOXES).join("second").exists(),
        "a refused create made a cgroup"
    );
    assert!(
        !node.container_dir(&second).exists(),
        "a refused create made state"
    );
    node.delete(&first).expect("delete");
    node.create(lab.running(second.clone(), &wait))
        .expect("a delete gives its place back");
    node.delete(&second).expect("delete");
}

/// AC-T7.1.1: the `oci` class through the node holds the runtime contract, HEX-RT-11 and 12 (the
/// init's exit) included, and the differential suite holds it to the simulator's trace (HEX-DIFF-1..6,
/// CLAUDE.md §8).
fn conformance_on_oci(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-conf"), state.path());
    let failures = hex_api::differential::run(&node, lab);
    assert!(failures.is_empty(), "conformance failures: {failures:#?}");
}

/// Decision 4 (goal 4 plan step 4): each box whose bundle makes a network namespace of its own runs in
/// a namespace no other box has had, loopback up (its flags, read in the box from the namespace's
/// sysfs, are IFF_UP | IFF_LOOPBACK), and the node counts where the namespace came from. Namespace
/// inode numbers are compared while every box named holds its own, since the kernel reuses a freed
/// one's.
/// - HEX-NETNS-9: a node whose pool holds none (bound 0) makes a box's namespace on the create's path
///   and counts it so: made on the path 1, nothing made ahead or taken.
/// - HEX-NETNS-10, rootless, the same node: the namespace is made in the box's own user namespace,
///   which owns it, as the box's init mounting its sysfs shows (a sysfs mount takes `CAP_SYS_ADMIN` in
///   the user namespace that owns the mounter's network namespace, `kobj_ns_current_may_mount`).
/// - HEX-NETNS-8, as root: once a node's pool holds two, two boxes take one each, their namespaces
///   differ from each other's, from the first box's and from the host's, and the node keeps no
///   descriptor of either: the pool's copy is dropped once handed over.
fn network_namespaces(lab: &Lab, own: &Path) {
    let host = netns_of(std::process::id());
    let dry_state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let dry = lab
        .node(&own.join("hex-oci-netns-dry"), dry_state.path())
        .with_netns_bound(0)
        .unwrap();
    let first = BoxId::new("netns-dry").unwrap();
    dry.create(lab.networked(first.clone())).expect("create");
    dry.start(&first).expect("start");
    let made = netns_of(init_pid(&dry.container_dir(&first)));
    assert_ne!(made, host, "the box's network namespace is its own");
    assert_eq!(loopback_flags(&dry, &first), "0x9", "its loopback is up");
    let counts = dry.netns().unwrap();
    assert_eq!(
        (counts.made_ahead, counts.taken, counts.made_on_path),
        (0, 0, 1),
        "(made ahead, taken, made on the path)"
    );
    if lab.rootless {
        dry.delete(&first).expect("delete");
        return;
    }

    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-netns"), state.path());
    wait_for(
        &TWO_NAMESPACES,
        || {
            let c = node.netns().unwrap();
            c.made_ahead >= c.taken + c.discarded + 2
        },
        "the node's pool to hold two namespaces",
    );
    let mut seen = vec![host, made];
    let mut taken = Vec::new();
    for name in ["netns-a", "netns-b"] {
        let id = BoxId::new(name).unwrap();
        node.create(lab.networked(id.clone())).expect("create");
        node.start(&id).expect("start");
        let ns = netns_of(init_pid(&node.container_dir(&id)));
        assert!(!seen.contains(&ns), "{name}'s {ns} is no other's: {seen:?}");
        assert!(
            !held_by_this_process(&ns),
            "the node keeps no descriptor of {name}'s {ns}"
        );
        assert_eq!(loopback_flags(&node, &id), "0x9", "{name}'s loopback is up");
        seen.push(ns);
        taken.push(id);
    }
    let counts = node.netns().unwrap();
    assert_eq!(
        (counts.taken, counts.made_on_path),
        (2, 0),
        "(taken, made on the path)"
    );
    for id in &taken {
        node.delete(id).expect("delete");
    }
    dry.delete(&first).expect("delete");
}

/// The network namespace process `pid` is in, as its `/proc` link names it (`net:[<inode>]`).
fn netns_of(pid: impl std::fmt::Display) -> String {
    std::fs::read_link(format!("/proc/{pid}/ns/net"))
        .unwrap()
        .to_string_lossy()
        .into_owned()
}

/// Whether this process, the node agent, holds a descriptor of network namespace `ns`.
fn held_by_this_process(ns: &str) -> bool {
    std::fs::read_dir("/proc/self/fd").unwrap().any(|fd| {
        std::fs::read_link(fd.unwrap().path()).is_ok_and(|link| link.to_string_lossy() == ns)
    })
}

/// Loopback's flags in box `id`'s network namespace, read in the box.
fn loopback_flags(rt: &dyn Runtime, id: &BoxId) -> String {
    let (mut out, w) = std::io::pipe().unwrap();
    let mut proc = rt
        .exec(
            id,
            hex_api::Program::of(&["/usr/bin/cat", "/sys/class/net/lo/flags"]),
            hex_api::Stdio {
                stdout: Some(w),
                ..hex_api::Stdio::null()
            },
        )
        .expect("exec cat");
    let mut flags = String::new();
    std::io::Read::read_to_string(&mut out, &mut flags).unwrap();
    assert_eq!(
        proc.wait_for(conformance::PATIENCE).unwrap(),
        Some(Exit::Code(0))
    );
    flags.trim().to_owned()
}

/// Goal 3 plan step 2: no process is made per exec but the program. Exec a non-hex `sleep` into a
/// running box through the seam, and find the exec'd program is the agent's own direct child — no
/// `hex __oci-exec` helper stands beside it, as one did before the seed (agent → helper → program; now
/// agent → program) — and that a second exec spawns no new `hex __oci-seed`: the seed is reused, so no
/// `hex` binary is exec'd per call.
fn program_alone(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-alone"), state.path());
    let id = BoxId::new("alone").unwrap();
    // A non-hex, long-lived init and program, so "no hex process beside it" is plain to see.
    node.create(lab.running(id.clone(), &["/usr/bin/sleep".to_owned(), "600".to_owned()]))
        .expect("create");
    node.start(&id).expect("start");
    let agent = std::process::id();

    let mut first = node
        .exec(
            &id,
            hex_api::Program::of(&["/usr/bin/sleep", "300"]),
            hex_api::Stdio::null(),
        )
        .expect("exec a program");
    let pid = first.pid();
    assert_eq!(
        first.try_wait().expect("try_wait"),
        None,
        "the program is still running while we look at it"
    );
    // It is the agent's own child: no `hex __oci-exec` helper stands between the agent and the program.
    // Cloned straight from the agent's thread (as root) it is born so; through a rootless node's seed,
    // the seed reaps its intermediate before the agent is told, so it has reparented by the time `exec`
    // returns.
    assert_eq!(
        ppid(pid),
        Some(agent),
        "the exec'd program's parent is the node agent itself"
    );
    let children = children_of(agent);
    assert!(
        !children.iter().any(|&c| cmdline_has(c, "__oci-exec")),
        "no hex __oci-exec helper runs beside the program"
    );
    // Goal 3 plan step 2's acceptance: no process is made per exec but the program, and no hex binary is
    // exec'd per call. A node that runs as root clones the program straight from its own thread; a
    // rootless one hands the exec to a reusable seed. Either way a second exec adds to the agent's
    // children its program alone (the diff below). A `hex __oci-seed` is up whichever way the node runs:
    // the box's create is made by a create seed as of goal 5, so the seed count no longer tells a root
    // node from a rootless one — the diff does.
    assert!(
        children.iter().any(|&c| cmdline_has(c, "__oci-seed")),
        "a create seed is up, having made the box"
    );
    // The node makes inits ahead for boxes of no owner, each the agent's child (decision 8): their refill is
    // waited out first, its queue full (or a make failed), so the diff is the exec's alone.
    wait_for(
        &AHEAD,
        || {
            let ahead = node.cgroup_reuse().unwrap().ahead;
            ahead.ready >= ahead.size || ahead.failed > 0
        },
        "the inits made ahead to fill their queue, or a make to fail",
    );
    let children = children_of(agent);
    let before: std::collections::BTreeSet<u32> = children.into_iter().collect();
    let second = node
        .exec(
            &id,
            hex_api::Program::of(&["/usr/bin/sleep", "300"]),
            hex_api::Stdio::null(),
        )
        .expect("a second exec");
    let after: std::collections::BTreeSet<u32> = children_of(agent).into_iter().collect();
    assert_eq!(
        after.difference(&before).copied().collect::<Vec<_>>(),
        [second.pid()],
        "the second exec added no process but its program: no helper, no seed, no hex binary exec'd"
    );
    assert_eq!(
        ppid(second.pid()),
        Some(agent),
        "the second program is the agent's child too"
    );

    node.delete(&id).expect("delete ends the programs");
    // Each program was a member of the box cgroup, so delete's cgroup.kill reached it: reap it (the
    // caller's own Proc, as HEX-RT-7 does) and find it ended by a signal, no zombie left.
    for mut p in [first, second] {
        let exit = p
            .wait_for(conformance::PATIENCE)
            .expect("wait for a deleted program")
            .expect("the program ended with the box");
        assert!(
            matches!(exit, Exit::Signal(_)),
            "delete ended the program by a signal: {exit:?}"
        );
    }
}

/// A program holds its stdio alone, whatever the agent above it holds (CVE-2024-21626's class; decision 3,
/// "Descriptors reach a program only on purpose"; AC-T7.1.4). Do: leave a descriptor open in this process, the
/// agent, without close-on-exec, as a window above a program would; start a box's program and exec a second into
/// it. Expect: each comes to hold nothing past 0, 1 and 2.
fn programs_hold_their_stdio_alone(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-stdio"), state.path());
    let id = BoxId::new("stdio").unwrap();
    let stray = rustix::fs::open(
        c"/dev/null",
        rustix::fs::OFlags::RDONLY,
        rustix::fs::Mode::empty(),
    )
    .unwrap();
    node.create(lab.running(id.clone(), &["/usr/bin/sleep".to_owned(), "600".to_owned()]))
        .expect("create");
    node.start(&id).expect("start");
    let mut exec = node
        .exec(
            &id,
            hex_api::Program::of(&["/usr/bin/sleep", "300"]),
            hex_api::Stdio::null(),
        )
        .expect("exec a program");
    for pid in [
        u32::try_from(init_pid(&node.container_dir(&id))).unwrap(),
        exec.pid(),
    ] {
        // Its loader holds a descriptor for a moment after the exec, so what's waited for is their end; a
        // stray stays.
        let held = || -> Vec<String> {
            std::fs::read_dir(format!("/proc/{pid}/fd"))
                .unwrap()
                .map(|fd| fd.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|fd| !["0", "1", "2"].contains(&fd.as_str()))
                .collect()
        };
        until(
            &LOADER,
            &format!("program {pid} to become its program and hold its stdio alone"),
            || cmdline_has(pid, "/usr/bin/sleep") && held().is_empty(),
        )
        .unwrap();
    }
    drop(stray);
    node.delete(&id).expect("delete ends the programs");
    exec.wait_for(conformance::PATIENCE)
        .expect("wait for a deleted program")
        .expect("the program ended with the box");
}

/// Goal 3 plan step 6: an exec into a running box reads nothing of the box's from disk; what it needs
/// of the container is kept with the init the agent watches. Move the container's state file away, exec
/// `true` into the box, and find it ran and exited 0; with the file back, the box deletes as usual.
fn exec_reads_no_state(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-nostate"), state.path());
    let id = BoxId::new("nostate").unwrap();
    node.create(lab.running(id.clone(), &Lab::act(&Act::Wait)))
        .expect("create");
    node.start(&id).expect("start");
    let file = node.container_dir(&id).join("state.json");
    let aside = node.container_dir(&id).join("state.json.aside");
    std::fs::rename(&file, &aside).expect("move the container's state away");
    let ran = node
        .exec(
            &id,
            hex_api::Program::of(&["/usr/bin/true"]),
            hex_api::Stdio::null(),
        )
        .and_then(|mut p| p.wait_for(conformance::PATIENCE));
    std::fs::rename(&aside, &file).expect("put the container's state back");
    assert_eq!(
        ran,
        Ok(Some(Exit::Code(0))),
        "exec ran its program with the container's state file gone"
    );
    node.delete(&id).expect("delete");
}

/// `/proc/<pid>/stat` field 4, the parent pid.
fn ppid(pid: u32) -> Option<u32> {
    stat_field(pid, 4)
}

/// `/proc/<pid>/stat` field `n` (4 the parent pid, 5 the process group, 6 the session, proc(5)), counted
/// from 1 and read past the parenthesized command name (field 2, which may itself hold spaces and
/// parentheses), as a number.
fn stat_field(pid: u32, n: usize) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (_, rest) = stat.rsplit_once(')')?;
    rest.split_whitespace().nth(n.checked_sub(3)?)?.parse().ok()
}

/// Whether any argument in `/proc/<pid>/cmdline` (NUL-separated) is exactly `needle`.
fn cmdline_has(pid: u32, needle: &str) -> bool {
    std::fs::read(format!("/proc/{pid}/cmdline"))
        .map(|b| b.split(|&c| c == 0).any(|a| a == needle.as_bytes()))
        .unwrap_or(false)
}

/// The processes whose parent is `agent`, by scanning `/proc`.
fn children_of(agent: u32) -> Vec<u32> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return out;
    };
    for entry in entries.flatten() {
        if let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>()
            && ppid(pid) == Some(agent)
        {
            out.push(pid);
        }
    }
    out
}

/// A box's program forks a child and exits, orphaning the child to the agent (its subreaper); the
/// child waits on a FIFO, so it ends when the test says. Once it has ended the sweep reaps it within
/// a [`hex_node::supervise::SWEEP_PERIOD`]: its pid goes, rather than stay a zombie of the agent.
fn orphans_are_reaped(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-orphan"), state.path());
    let fifo = state.path().join("go");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let id = BoxId::new("orphaner").unwrap();
    node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Process,
        owner: None,
        band: 0,
    })
    .unwrap();
    let (mut out, w) = std::io::pipe().unwrap();
    // The orphan's stdout and stderr go for good (`exec`) before the FIFO opens (which blocks), so it never
    // holds the pipe the test reads to its end. A builtin's own redirections wouldn't do: dash keeps a copy
    // of each fd they replace, to restore it after (dash 0.5.12 `src/eval.c:836-837`, `src/redir.c:134-155`,
    // `:444-449`), so `read x >/dev/null < go &` holds the pipe while it waits.
    let script = format!(
        "(exec >/dev/null 2>&1; read x < {}) & echo $!",
        fifo.display()
    );
    let mut proc = node
        .exec(
            &id,
            hex_api::Program::of(&["/bin/sh", "-c", &script]),
            hex_api::Stdio {
                stdout: Some(w),
                ..hex_api::Stdio::null()
            },
        )
        .unwrap();
    assert_eq!(
        proc.wait_for(conformance::PATIENCE).unwrap(),
        Some(Exit::Code(0))
    );
    // Read to the pipe's end on a thread of its own, within the patience: an orphan still holding the pipe
    // fails the test by name rather than hang it.
    let (got, read) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut text = String::new();
        let _ = got.send(std::io::Read::read_to_string(&mut out, &mut text).map(|_| text));
    });
    // The program ended (waited for above), so its stdout ends with it: within a program's run's record.
    let ended = PROGRAM.saturating_mul(2);
    let text = read
        .recv_timeout(ended)
        .unwrap_or_else(|_| {
            panic!(
                "the program's stdout didn't end within {ended:?}: the orphan holds the pipe the test reads"
            )
        })
        .unwrap();
    let orphan: i32 = text.trim().parse().unwrap();
    assert_eq!(
        parent_of(orphan),
        i32::try_from(std::process::id()).unwrap(),
        "the orphan is the agent's"
    );
    std::fs::write(&fifo, b"go\n").unwrap();
    wait_for(
        &ORPHAN_REAPED,
        || !Path::new(&format!("/proc/{orphan}")).exists(),
        "the ended orphan to be reaped",
    );
    node.delete(&id).unwrap();
}

/// Goal 5 plan step 2: no create waits on another box's record write, its fsyncs included. 32 boxes
/// are created at once from 32 threads; then 31 more while a box with a durable record (an `OciSpec`
/// box, its rendered bundle on disk) is held inside its record write, made once its container is (goal
/// 5 extended step 5): its temp file is a FIFO no one has opened, so the write can't go on until this
/// test reads it. All 31 are made while it's held,
/// and with the node's container state on tmpfs none of their records is synced. Let go, the held
/// write fails at its fsync (a FIFO can't be synced), which shows it was a durable one. Both runs'
/// latencies are printed: the 31 beside the held write against the 32 with none.
fn held_record_write(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-held"), state.path());
    let wait = Lab::act(&Act::Wait);
    let specs = |prefix: &str, n: usize| -> Vec<BoxSpec> {
        (0..n)
            .map(|i| lab.running(BoxId::new(&format!("{prefix}{i}")).unwrap(), &wait))
            .collect()
    };
    // Each create timed on a thread of its own, all started at once; their latencies in the order they
    // came, a create that didn't come within the patience missing.
    let create_all = |specs: Vec<BoxSpec>| -> Vec<Duration> {
        let n = specs.len();
        let (done, made) = std::sync::mpsc::channel();
        std::thread::scope(|s| {
            for spec in specs {
                let (node, done) = (&node, done.clone());
                s.spawn(move || {
                    let t = Instant::now();
                    let made = node.create(spec).map(|_| t.elapsed());
                    let _ = done.send(made);
                });
            }
            (0..n)
                .map_while(|_| made.recv_timeout(CREATE.saturating_mul(2)).ok())
                .map(|m| m.expect("create"))
                .collect()
        })
    };

    let free = specs("free", 32);
    let ids: Vec<BoxId> = free.iter().map(|s| s.id.clone()).collect();
    let alone = create_all(free);
    assert_eq!(alone.len(), 32, "32 creates at once, none held");
    for id in &ids {
        node.delete(id).unwrap();
    }

    let fifo = state.path().join(".held.tmp");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    let held = BoxId::new("held").unwrap();
    let held_spec = BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: held.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Oci(Box::new(OciSpec {
            rootfs: Rootfs::Dir(lab.rootfs.clone()),
            init: hex_api::Program::of(&["/hex", "selftest-box", "wait"]),
            mounts: Vec::new(),
            hostname: None,
            network: Network::Host,
            hosts: Vec::new(),
        })),
        owner: None,
        band: 0,
    };
    let busy = specs("busy", 31);
    let ids: Vec<BoxId> = busy.iter().map(|s| s.id.clone()).collect();
    let synced = node.record_syncs();
    // Nothing here panics before the held write is let go, so a create that waited on it ends too and
    // the scope's joins return.
    let (beside, synced_beside, held_made) = std::thread::scope(|s| {
        let holder = s.spawn(|| node.create(held_spec));
        wait_for(
            &CREATING,
            blocked_opening_a_fifo,
            "the held create to block in its record write",
        );
        let (done, made) = std::sync::mpsc::channel();
        for spec in busy {
            let (node, done) = (&node, done.clone());
            s.spawn(move || {
                let t = Instant::now();
                let made = node.create(spec).map(|_| t.elapsed());
                let _ = done.send(made);
            });
        }
        let beside: Vec<Result<Duration, Refusal>> = (0..31)
            .map_while(|_| made.recv_timeout(CREATE.saturating_mul(2)).ok())
            .collect();
        let synced_beside = node.record_syncs();
        // Let the held write go on: its open completes, and it writes the record into the FIFO.
        let mut held_bytes = Vec::new();
        let read = std::fs::File::open(&fifo).and_then(|mut f| f.read_to_end(&mut held_bytes));
        (beside, synced_beside, (read, holder.join().unwrap()))
    });
    let (read, held_made) = held_made;
    read.unwrap();
    std::fs::remove_file(&fifo).unwrap();
    assert_eq!(
        beside.len(),
        31,
        "every create beside the held write was made while it was held"
    );
    let beside: Vec<Duration> = beside.into_iter().map(|m| m.expect("create")).collect();
    assert!(
        matches!(
            &held_made,
            Err(Refusal::Os {
                op: "flushing a record",
                ..
            })
        ),
        "the held write, let go, failed at its fsync: {held_made:?}"
    );
    if on_tmpfs(node.oci_root()) {
        assert_eq!(
            synced_beside, synced,
            "the 31 boxes' records, their container state on tmpfs alone, were not synced"
        );
    } else {
        eprintln!(
            "held_record_write: the node's container state isn't on tmpfs here, so its boxes' records are synced too"
        );
    }
    let ms = |d: &Duration| d.as_secs_f64() * 1e3;
    let (alone, beside) = (sorted_ms(&alone), sorted_ms(&beside));
    eprintln!(
        "held_record_write: create p50 {:.3} ms, max {:.3} ms with 32 at once; p50 {:.3} ms, max {:.3} ms with 31 beside a held durable write",
        ms(&alone[alone.len() / 2]),
        ms(&alone[alone.len() - 1]),
        ms(&beside[beside.len() / 2]),
        ms(&beside[beside.len() - 1]),
    );
    for id in &ids {
        node.delete(id).unwrap();
    }
    assert_eq!(node.inspect(&held), Err(Refusal::NoSuchBox(held.clone())));
    assert!(node.list().unwrap().is_empty(), "a box is left");
}

/// Goal 5 plan step 2: two nodes on one host, each with a state directory of its own, keep their
/// containers' state in the one root under the runtime directory; a box of the same id in each gets a
/// container of its own, named for its node. Both are made and start, each its own init; deleting one
/// leaves the other running, its state where it was.
fn two_nodes_one_box_id(lab: &Lab, own: &Path) {
    let (state_a, state_b) = (
        tempfile::tempdir_in(lab.bundles.path()).unwrap(),
        tempfile::tempdir_in(lab.bundles.path()).unwrap(),
    );
    let a = lab.node(&own.join("hex-oci-two-a"), state_a.path());
    let b = lab.node(&own.join("hex-oci-two-b"), state_b.path());
    let id = BoxId::new("same").unwrap();
    let wait = Lab::act(&Act::Wait);
    a.create(lab.running(id.clone(), &wait))
        .expect("create in a");
    b.create(lab.running(id.clone(), &wait))
        .expect("create in b");
    let (in_a, in_b) = (a.container_dir(&id), b.container_dir(&id));
    assert_ne!(in_a.file_name(), in_b.file_name(), "two containers' names");
    if a.oci_root() != b.oci_root() {
        eprintln!(
            "two_nodes_one_box_id: no runtime directory here, so each node keeps its containers in its state directory"
        );
    }
    for node in [&a, &b] {
        node.start(&id).expect("start");
        assert_eq!(node.inspect(&id).unwrap().state, BoxState::Running);
    }
    assert_ne!(init_pid(&in_a), init_pid(&in_b), "two inits");
    a.delete(&id).expect("delete in a");
    assert!(!in_a.exists(), "a's container state is left");
    assert_eq!(
        b.inspect(&id).unwrap().state,
        BoxState::Running,
        "b's box runs on"
    );
    assert!(
        in_b.join("state.json").exists(),
        "b's container state is kept"
    );
    b.delete(&id).expect("delete in b");
    assert!(!in_b.exists(), "b's container state is left");
}

/// What a call made while another box's create is held does with the same id.
#[derive(Clone, Copy, Debug)]
enum Second {
    Create,
    Delete,
}

/// AC-T7.1.1, T-T7.1.4 (a time-of-check-to-time-of-use hole, found 2026-11-04): the runtime parses and checks the
/// spec in a bundle's config.json, and whatever then starts the container's init, a create seed or the shim, must
/// make the container from that spec, never from a later read of the bundle. Do: hold a create reading its config.json
/// (a FIFO); once the agent has it open, give the agent the lab's spec and put in its place, for any later read, the
/// same spec holding CAP_SYS_ADMIN, which rootless (a user namespace on the host's network) spec.rs refuses. Through
/// the node twice: a seed starts the init; and, the spec asking for a createRuntime hook, the shim does as root (the
/// shim's one consumer, a spec a seed can't serve; rootless a seed serves it too). Expect: the create refused, or its
/// init without CAP_SYS_ADMIN, as the spec the runtime checked has it.
fn a_config_swapped_after_the_check_never_makes_the_container(lab: &Lab, own: &Path) {
    let wait = Lab::act(&Act::Wait);
    let checked = lab.config(&wait, false);
    let swapped = checked.replace(r#""CAP_CHOWN""#, r#""CAP_SYS_ADMIN", "CAP_CHOWN""#);
    assert_ne!(checked, swapped, "the swapped spec names CAP_SYS_ADMIN");
    let held_bundle = || {
        let bundle = tempfile::Builder::new()
            .prefix("swap-")
            .tempdir_in(lab.bundles.path())
            .unwrap()
            .keep();
        assert!(
            Command::new("mkfifo")
                .arg(bundle.join("config.json"))
                .status()
                .unwrap()
                .success()
        );
        bundle
    };
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();

    // Through the node: a seed starts the init.
    let node = lab.node(&own.join("hex-oci-swap"), state.path());
    let id = BoxId::new("swap-node").unwrap();
    let bundle = held_bundle();
    let config = bundle.join("config.json");
    let made = std::thread::scope(|s| {
        let made = s.spawn(|| {
            node.create(BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: id.clone(),
                bounds: Bounds::default(),
                class: ClassSpec::Bundle(bundle.clone()),
                owner: None,
                band: 0,
            })
        });
        wait_for(
            &CREATING,
            blocked_opening_a_fifo,
            "the create to wait reading its config",
        );
        swap_the_held_config(&config, &checked, &swapped);
        made.join().unwrap()
    });
    if made.is_ok() {
        let pid = u32::try_from(init_pid(&node.container_dir(&id))).unwrap();
        let held = holds_sys_admin(pid);
        node.delete(&id).unwrap();
        assert!(
            !held,
            "the node's container was made from the swapped spec, not the one checked"
        );
    }

    // Through the node again, a createRuntime hook routing the create through the shim as root.
    let hooked = |spec: &str| {
        spec.replacen(
            "\n  \"linux\":",
            "\n  \"hooks\": {\"createRuntime\": [{\"path\": \"/usr/bin/true\", \"args\": [\"true\"]}]},\n  \"linux\":",
            1,
        )
    };
    let (checked, swapped) = (hooked(&checked), hooked(&swapped));
    assert!(checked.contains("createRuntime"), "the hooked spec");
    let id = BoxId::new("swap-shim").unwrap();
    let bundle = held_bundle();
    let config = bundle.join("config.json");
    let through_shim = node.creates_through_shim();
    let made = std::thread::scope(|s| {
        let made = s.spawn(|| {
            node.create(BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: id.clone(),
                bounds: Bounds::default(),
                class: ClassSpec::Bundle(bundle.clone()),
                owner: None,
                band: 0,
            })
        });
        wait_for(
            &CREATING,
            blocked_opening_a_fifo,
            "the shim's create to wait reading its config",
        );
        swap_the_held_config(&config, &checked, &swapped);
        made.join().unwrap()
    });
    if made.is_ok() {
        if !lab.rootless {
            assert_eq!(
                node.creates_through_shim(),
                through_shim + 1,
                "a createRuntime hook routes the create through the shim"
            );
        }
        let pid = u32::try_from(init_pid(&node.container_dir(&id))).unwrap();
        let held = holds_sys_admin(pid);
        node.delete(&id).unwrap();
        assert!(
            !held,
            "the shim's container was made from the swapped spec, not the one checked"
        );
    }
}

/// AC-T7.1.1, T-T7.1.4 (the same class as the create's hole, found 2026-11-04): an exec takes its program's process
/// settings from the spec its box's create checked, never the bundle's config.json as it is by then. Do: make and
/// start a box from the lab's spec; then write in its bundle the same spec holding CAP_SYS_ADMIN (which rootless, in a
/// user namespace on the host's network, the checks refuse); exec a program. Expect: the program runs, without
/// CAP_SYS_ADMIN, as the checked spec has it. Then, as a box made before its spec was kept: its kept spec removed and
/// a spec the checks refuse in its bundle. Expect: the exec refused by type, never run from that spec unchecked.
fn an_exec_takes_the_checked_spec_not_the_bundle_swapped_after_create(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-swap-exec"), state.path());
    let id = BoxId::new("swap-exec").unwrap();
    let sleep = ["/usr/bin/sleep".to_owned(), "600".to_owned()];
    let spec = lab.running(id.clone(), &sleep);
    let ClassSpec::Bundle(bundle) = &spec.class else {
        panic!("lab.running makes a bundle")
    };
    let bundle = bundle.clone();
    node.create(spec).expect("create");
    node.start(&id).expect("start");
    let checked = std::fs::read_to_string(bundle.join("config.json")).unwrap();
    let swapped = checked.replace(r#""CAP_CHOWN""#, r#""CAP_SYS_ADMIN", "CAP_CHOWN""#);
    assert_ne!(checked, swapped, "the swapped spec names CAP_SYS_ADMIN");
    std::fs::write(bundle.join("config.json"), &swapped).unwrap();
    let exec = node.exec(
        &id,
        hex_api::Program::of(&["/usr/bin/sleep", "300"]),
        hex_api::Stdio::null(),
    );
    let held = exec.as_ref().ok().map(|exec| {
        let pid = exec.pid();
        wait_for(
            &EXECUTING,
            || cmdline_has(pid, "/usr/bin/sleep"),
            "the exec'd program to run",
        );
        holds_sys_admin(pid)
    });
    drop(exec);
    // A box made before its spec was kept, its bundle's spec one the checks refuse.
    match std::fs::remove_file(node.container_dir(&id).join("checked.json")) {
        Ok(()) => {}
        // A tree that keeps no spec (before AC-T7.1.1's fix): the box is an older one as it is.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => panic!("removing the kept spec: {e}"),
    }
    std::fs::write(bundle.join("config.json"), r#"{"ociVersion":""}"#).unwrap();
    let older = node.exec(
        &id,
        hex_api::Program::of(&["/usr/bin/sleep", "300"]),
        hex_api::Stdio::null(),
    );
    let older_ran = older.is_ok();
    drop(older);
    node.delete(&id).expect("delete");
    assert_eq!(
        held,
        Some(false),
        "the exec'd program held the swapped spec's capabilities, or didn't run"
    );
    assert!(
        !older_ran,
        "an exec ran from a bundle spec the checks refuse"
    );
}

/// AC-T7.1.1, T-T7.1.4: a start's poststart hooks, which the agent runs on the host, are the ones the box's create
/// checked, never the bundle's config.json's as it is by then. Do: make a box whose spec names a poststart hook that
/// marks file A; then write in its bundle the same spec, its hook marking file B; start the box. Expect: A marked, B
/// not.
fn a_start_runs_the_checked_specs_hooks_not_the_bundle_swapped_after_create(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-swap-start"), state.path());
    let id = BoxId::new("swap-start").unwrap();
    let marks = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let (checked_mark, swapped_mark) = (marks.path().join("checked"), marks.path().join("swapped"));
    let with_hook = |mark: &Path| {
        let config = lab.config(&Lab::act(&Act::Wait), false);
        let end = config.rfind('}').unwrap();
        format!(
            r#"{},"hooks":{{"poststart":[{{"path":"/bin/sh","args":["sh","-c","touch {}"]}}]}}}}"#,
            config.get(..end).unwrap().trim_end(),
            mark.display()
        )
    };
    let bundle = tempfile::Builder::new()
        .prefix("swap-start-")
        .tempdir_in(lab.bundles.path())
        .unwrap()
        .keep();
    std::fs::write(bundle.join("config.json"), with_hook(&checked_mark)).unwrap();
    node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Bundle(bundle.clone()),
        owner: None,
        band: 0,
    })
    .expect("create");
    std::fs::write(bundle.join("config.json"), with_hook(&swapped_mark)).unwrap();
    let started = node.start(&id);
    node.delete(&id).expect("delete");
    started.expect("start");
    assert!(
        checked_mark.exists(),
        "the checked spec's poststart hook didn't run"
    );
    assert!(
        !swapped_mark.exists(),
        "the swapped spec's poststart hook ran on the host"
    );
}

/// Goal 5 extended step 5 (T-T7.1.2): a box being made has no record yet, so a second create of its id,
/// or a delete, waits for the create under way rather than make or remove parts at the paths the id
/// fixes. Do: hold a create reading its bundle's config.json, a FIFO no one has opened, once its cgroup
/// and its container's state directory are made; while it's held, create the same id from a second
/// thread, or delete it; then let the first go ([`let_the_held_create_go`]). Expect: the second
/// call waits (the node counts it) until the first is done; the first makes its box whole; then the
/// second create is refused as `Exists`, the box running on as made, or the delete removes the box,
/// leaving nothing of it.
fn a_create_or_delete_of_a_box_being_made_waits_for_it(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let box_root = own.join("hex-oci-turns");
    let node = lab.node(&box_root, state.path());
    let wait = Lab::act(&Act::Wait);
    for second in [Second::Create, Second::Delete] {
        let id = BoxId::new(&format!("being-made-{second:?}").to_lowercase()).unwrap();
        let bundle = tempfile::Builder::new()
            .prefix("held-")
            .tempdir_in(lab.bundles.path())
            .unwrap()
            .keep();
        let config = bundle.join("config.json");
        assert!(
            Command::new("mkfifo")
                .arg(&config)
                .status()
                .unwrap()
                .success()
        );
        let spec = BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(bundle),
            owner: None,
            band: 0,
        };
        let waited = node.waited_for_a_box();
        let (made, then, both_waited) = std::thread::scope(|s| {
            let first = s.spawn(|| node.create(spec));
            wait_for(
                &CREATING,
                blocked_opening_a_fifo,
                "the create to wait reading its config",
            );
            let then = s.spawn(|| match second {
                Second::Create => node.create(lab.running(id.clone(), &wait)).map(|_| ()),
                Second::Delete => node.delete(&id),
            });
            // The second waits, or (a node without the turn) ends at once; either way the first is let
            // go before anything is asserted, so a failure is reported rather than leaving it blocked.
            wait_for(
                &TURN,
                || node.waited_for_a_box() > waited || then.is_finished(),
                "the second call to wait for the create under way, or end",
            );
            let both_waited = !first.is_finished() && !then.is_finished();
            let_the_held_create_go(&config, &lab.config(&wait, false));
            (first.join().unwrap(), then.join().unwrap(), both_waited)
        });
        assert!(both_waited, "{second:?}: both wait: {then:?}");
        assert_eq!(
            node.waited_for_a_box(),
            waited + 1,
            "{second:?}: one call waited"
        );
        // The held create makes its box whole; what the second call then finds is that box.
        let made = made.expect("the held create makes its box");
        assert_eq!(
            made.state,
            BoxState::Created,
            "the held create made its box"
        );
        match second {
            Second::Create => {
                assert_eq!(
                    then,
                    Err(Refusal::Exists(id.clone())),
                    "the second create sees the box"
                );
                assert_eq!(
                    node.inspect(&id).unwrap().state,
                    BoxState::Created,
                    "the box runs on as made"
                );
                node.delete(&id).unwrap();
            }
            Second::Delete => {
                assert_eq!(then, Ok(()), "the delete removes the box the create made");
            }
        }
        assert_eq!(
            node.inspect(&id),
            Err(Refusal::NoSuchBox(id.clone())),
            "a record is left"
        );
        assert!(
            !box_root.join(OCI_BOXES).join(id.as_str()).exists(),
            "{id}'s cgroup is left"
        );
        assert!(
            !node.container_dir(&id).exists(),
            "{id}'s container state is left"
        );
    }
}

/// Goal 5 extended step 5's turn with step 6's reuse on (T-T7.1.2; the turn in place of step 6's claim, the lead's
/// call of 2026-11-04): an owned box's create takes a slot-named cgroup, so while it's being made no cgroup and no
/// record carry its id, and only its turn says it's being made. Do: hold the create of a box of owner O reading its
/// bundle's config.json (a FIFO no one has opened); while it's held, create the same id for O from a second thread;
/// then let the first go. Expect: the second waits for the first (the node counts it); once the first has made the box
/// in a slot, the second is refused as `Exists`; one slot is taken, never two: one more slot under the oci boxes' root
/// and one cgroup given out (fresh or handed on).
fn a_second_create_of_an_owned_box_being_made_waits_and_takes_no_slot(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let box_root = own.join("hex-oci-turns-owned");
    let node = lab.node(&box_root, state.path());
    let owner = BoxOwner::named("owner-turns").unwrap();
    let id = BoxId::new("being-made-owned").unwrap();
    let wait = Lab::act(&Act::Wait);
    let bundle = tempfile::Builder::new()
        .prefix("held-")
        .tempdir_in(lab.bundles.path())
        .unwrap()
        .keep();
    let config = bundle.join("config.json");
    assert!(
        Command::new("mkfifo")
            .arg(&config)
            .status()
            .unwrap()
            .success()
    );
    let slots = || {
        std::fs::read_dir(box_root.join(OCI_BOXES))
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| hex_core::reuse::slot_of(&e.file_name().to_string_lossy()).is_some())
            .count()
    };
    let given = |n: &Node| {
        let c = n.cgroup_reuse().unwrap();
        c.pool.fresh + c.pool.handed_on
    };
    let (slots_before, given_before) = (slots(), given(&node));
    let waited = node.waited_for_a_box();
    let spec = BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Bundle(bundle),
        owner: Some(owner.clone()),
        band: 0,
    };
    let (made, then, both_waited) = std::thread::scope(|s| {
        let first = s.spawn(|| node.create(spec));
        wait_for(
            &CREATING,
            blocked_opening_a_fifo,
            "the create to wait reading its config",
        );
        let then = s.spawn(|| {
            node.create(BoxSpec {
                route: None,
                allocation: None,
                devices: Vec::new(),
                id: id.clone(),
                bounds: Bounds::default(),
                class: ClassSpec::Bundle(lab.bundle(&wait)),
                owner: Some(owner.clone()),
                band: 0,
            })
        });
        // The second waits, or (a node without the turn) ends at once; either way the first is let go below,
        // so a failure is reported rather than leaving it blocked.
        wait_for(
            &TURN,
            || node.waited_for_a_box() > waited || then.is_finished(),
            "the second create to wait for the one under way, or end",
        );
        let both_waited = !first.is_finished() && !then.is_finished();
        let_the_held_create_go(&config, &lab.config(&wait, false));
        (first.join().unwrap(), then.join().unwrap(), both_waited)
    });
    assert!(
        both_waited,
        "the second create waits for the one under way: {then:?}"
    );
    assert_eq!(node.waited_for_a_box(), waited + 1, "one create waited");
    let made = made.expect("the held create makes its box");
    assert_eq!(
        made.state,
        BoxState::Created,
        "the held create made its box"
    );
    assert_eq!(
        then.map(|_| ()),
        Err(Refusal::Exists(id.clone())),
        "the second create sees the box"
    );
    let dir = node.cgroup_dir(&id).expect("the box's cgroup");
    let name = dir.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        hex_core::reuse::slot_of(&name).is_some(),
        "the box's cgroup is a slot: {name}"
    );
    assert_eq!(slots(), slots_before + 1, "one slot more, not two");
    assert_eq!(given(&node), given_before + 1, "one cgroup given out");
    if node.inspect(&id).is_ok() {
        node.delete(&id).unwrap();
    }
    assert_eq!(
        node.inspect(&id),
        Err(Refusal::NoSuchBox(id.clone())),
        "nothing of the box is left"
    );
}

/// §4, goal 5 extended step 5's follow-on fix: a delete of a box while a create of the same id is under
/// way must leave the node consistent, whatever outcome the node lawfully gives. The create's cgroup, made
/// first, claims the id, but with no record until the box is whole; a delete that arrives while the create
/// is held could take that cgroup for a made box and scrub it, so the create then builds into the removed
/// cgroup and is refused, or writes a Ready record over nothing. Force the race by a fact, not a sleep:
/// hold a create reading its bundle's config.json (a FIFO no one has opened), its cgroup already made; run
/// a delete of the same id; wait until the delete has had its chance to race — the cgroup gone (the
/// un-fixed node scrubs it) or a wait for the create's turn registered (the fixed node) — then let the
/// create go ([`let_the_held_create_go`]). Over a few rounds, require: in each, the delete waited for the
/// create's turn (the raced path ran, and the fix is present — non-vacuity); a lawful result (the create
/// made the box or the node refused it by type, never refused building into a scrubbed cgroup; the delete
/// removed the box or found none); and nothing of the box left in any place, no Ready over nothing; and
/// across them, the create making its box and the delete removing it at least once (non-vacuity). Without the per-box turn the delete does not wait — it scrubs the
/// create's cgroup — so `waited_for_a_box` never rises and the test fails. The turn (the follow-on fix)
/// makes the delete wait for the create and then remove the box it made.
fn a_delete_racing_a_create_of_one_id_leaves_the_node_consistent(lab: &Lab, own: &Path) {
    /// Rounds of the race, each a box of its own: enough that the box-made outcome is seen (below).
    const ROUNDS: usize = 3;
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let box_root = own.join("hex-oci-delete-race");
    let node = lab.node(&box_root, state.path());
    let wait = Lab::act(&Act::Wait);
    let mut made_then_removed = 0usize;
    for round in 0..ROUNDS {
        let id = BoxId::new(&format!("delete-race-{round}")).unwrap();
        let bundle = tempfile::Builder::new()
            .prefix("held-")
            .tempdir_in(lab.bundles.path())
            .unwrap()
            .keep();
        let config = bundle.join("config.json");
        assert!(
            Command::new("mkfifo")
                .arg(&config)
                .status()
                .unwrap()
                .success()
        );
        let cgroup = box_root.join(OCI_BOXES).join(id.as_str());
        let spec = BoxSpec {
            route: None,
            allocation: None,
            devices: Vec::new(),
            id: id.clone(),
            bounds: Bounds::default(),
            class: ClassSpec::Bundle(bundle),
            owner: None,
            band: 0,
        };
        let waited = node.waited_for_a_box();
        let (created, deleted) = std::thread::scope(|s| {
            let create = s.spawn(|| node.create(spec));
            wait_for(
                &CREATING,
                blocked_opening_a_fifo,
                "the create to block reading its config",
            );
            let delete = s.spawn(|| node.delete(&id));
            // Wait on a fact, not a clock: the delete has had its chance to race the held create — it took
            // the box's cgroup (scrubbed it: the un-fixed node), it waited for the create's turn (the fixed
            // node), or it ended. Only then is the create let go, so the race is always forced, and it is let
            // go before anything is asserted, so a failure is reported rather than leaving it blocked.
            wait_for(
                &TURN,
                || !cgroup.exists() || node.waited_for_a_box() > waited || delete.is_finished(),
                "the delete to take the box's cgroup, wait for the create's turn, or end",
            );
            let_the_held_create_go(&config, &lab.config(&wait, false));
            (create.join().unwrap(), delete.join().unwrap())
        });
        // Non-vacuity, and the fix: the delete waited for the create's turn. The un-fixed node does not
        // wait — it scrubs the create's half-made cgroup — so this stays at `waited` there and the test
        // fails, which is the bug. A run where the delete never contended (it ran after the create) also
        // leaves it unchanged, so that fails too: the raced path never ran.
        assert!(
            node.waited_for_a_box() > waited,
            "round {round}: the delete did not wait for the create's turn (the raced path never ran, or the \
             fix is absent): created {created:?}, deleted {deleted:?}"
        );
        // Every lawful outcome: the create made its box, or the node refused it by type; the delete removed
        // the box, or found none. The delete waited (above), so the create was never refused building into a
        // cgroup a delete scrubbed from under it.
        if let Ok(made) = &created {
            assert_eq!(
                made.state,
                BoxState::Created,
                "round {round}: the create made its box"
            );
        }
        match &deleted {
            Ok(()) => {
                if created.is_ok() {
                    made_then_removed += 1;
                }
            }
            Err(refusal) => assert!(
                matches!(refusal, Refusal::NoSuchBox(_)),
                "round {round}: the delete was refused unlawfully: {refusal:?}"
            ),
        }
        // Consistent in every case: nothing of the box left in any place, and no Ready over nothing.
        assert_eq!(
            node.inspect(&id),
            Err(Refusal::NoSuchBox(id.clone())),
            "round {round}: a record is left"
        );
        assert!(
            node.list().unwrap().is_empty(),
            "round {round}: a box is left"
        );
        assert!(!cgroup.exists(), "round {round}: its cgroup is left");
        assert!(
            !node.container_dir(&id).exists(),
            "round {round}: its container state is left"
        );
    }
    // Non-vacuity of the outcomes (CLAUDE.md §6): the box-made outcome, the create making its box and the
    // waiting delete removing it, ran at least once across the rounds; refusals alone would leave it untried.
    assert!(
        made_then_removed >= 1,
        "in none of {ROUNDS} rounds did the create make its box and the waiting delete remove it"
    );
}

/// T-T7.1.2 (goal 5 extended step 5): a node restarted over records in both its places. Do: make two
/// boxes whose directories all lie on tmpfs (from bundles, their container state under the runtime
/// directory) and one from an `OciSpec` of layers (its rendered bundle's writable layer on disk, in the
/// state directory); end one
/// tmpfs box's init; then open the node again on the same directories and recover. Expect: each record
/// where it belongs, before the restart and after it (the tmpfs boxes' among the runtime directory's
/// records, unsynced; the disk box's in the state directory, fsynced, file and directory); the live
/// boxes taken back and the one whose init ended removed; and, once the two are deleted (the disk
/// box's record synced again as it's marked going), nothing of any box left in either place.
fn records_in_both_places_are_found_after_a_restart(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let box_root = own.join("hex-oci-places");
    let id = |n: &str| BoxId::new(n).unwrap();
    let (on_tmpfs_live, on_tmpfs_ended, on_disk) =
        (id("on-tmpfs-live"), id("on-tmpfs-ended"), id("on-disk"));
    let node = lab.node(&box_root, state.path());
    let runtime = node.oci_root().to_owned();
    if !on_tmpfs(&runtime) {
        eprintln!(
            "SKIP records_in_both_places_are_found_after_a_restart: the runtime directory {} isn't tmpfs here",
            runtime.display()
        );
        return;
    }
    let records = records_dir(&runtime, state.path()).unwrap();
    // The records in a place, by file name: in it, or in its shards for the runtime record directory's
    // (goal 5 extended, step 6).
    let kept = |dir: &Path| -> BTreeSet<String> {
        let mut out = BTreeSet::new();
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.ends_with(".json") {
                out.insert(name);
            } else if entry.file_type().unwrap().is_dir() && name.len() == 2 {
                for inner in std::fs::read_dir(entry.path()).unwrap() {
                    let inner = inner.unwrap().file_name().to_string_lossy().into_owned();
                    if inner.ends_with(".json") {
                        out.insert(inner);
                    }
                }
            }
        }
        out
    };
    let names = |ids: &[&BoxId]| -> BTreeSet<String> {
        ids.iter().map(|id| format!("{id}.json")).collect()
    };
    let wait = Lab::act(&Act::Wait);
    for id in [&on_tmpfs_live, &on_tmpfs_ended] {
        node.create(lab.running(id.clone(), &wait))
            .expect("create a box on tmpfs");
    }
    assert_eq!(
        node.record_syncs(),
        hex_node::store::Syncs::default(),
        "the tmpfs boxes' records aren't synced"
    );
    node.create(BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: on_disk.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Oci(Box::new(OciSpec {
            rootfs: Rootfs::Layers(vec![lab.rootfs.clone()]),
            init: hex_api::Program::of(&["/hex", "selftest-box", "wait"]),
            mounts: Vec::new(),
            hostname: None,
            network: Network::Host,
            hosts: Vec::new(),
        })),
        owner: None,
        band: 0,
    })
    .expect("create a box with its bundle's writable layer on disk");
    assert_eq!(
        node.record_syncs(),
        hex_node::store::Syncs { files: 1, dirs: 1 },
        "the disk box's record is synced, file and directory"
    );
    let placed = || (kept(&records), kept(state.path()));
    assert_eq!(
        placed(),
        (
            names(&[&on_tmpfs_live, &on_tmpfs_ended]),
            names(&[&on_disk])
        ),
        "each record in its place"
    );
    node.signal(&on_tmpfs_ended, hex_api::Signal::KILL).unwrap();
    wait_exit(&node, &on_tmpfs_ended);
    drop(node);

    let node = lab.node(&box_root, state.path());
    let summary = node.recover().expect("recover");
    let adopted: BTreeSet<BoxId> = summary.adopted.iter().cloned().collect();
    let removed: BTreeSet<BoxId> = summary.removed.iter().map(|(id, _)| id.clone()).collect();
    assert_eq!(
        (adopted, removed),
        (
            [on_tmpfs_live.clone(), on_disk.clone()]
                .into_iter()
                .collect(),
            [on_tmpfs_ended.clone()].into_iter().collect()
        ),
        "{summary:?}"
    );
    assert_eq!(
        placed(),
        (names(&[&on_tmpfs_live]), names(&[&on_disk])),
        "each box taken back found where it belongs, the removed one in neither place"
    );
    for id in [&on_tmpfs_live, &on_disk] {
        assert_eq!(node.inspect(id).unwrap().state, BoxState::Created, "{id}");
        node.delete(id).unwrap();
    }
    assert_eq!(
        node.record_syncs(),
        hex_node::store::Syncs { files: 1, dirs: 1 },
        "the disk box's record, marked going, is synced again"
    );
    assert_eq!(
        placed(),
        (BTreeSet::new(), BTreeSet::new()),
        "a record is left"
    );
    for id in [&on_tmpfs_live, &on_tmpfs_ended, &on_disk] {
        assert!(
            !box_root.join(OCI_BOXES).join(id.as_str()).exists(),
            "{id}'s cgroup is left"
        );
        assert!(
            !node.container_dir(id).exists(),
            "{id}'s container state is left"
        );
    }
    assert!(
        !state.path().join("bundles").join(on_disk.as_str()).exists(),
        "the disk box's bundle is left"
    );
}

/// Goal 5 plan step 4 (T-T7.1.4), the lab test as root and rootless: a container made through a seed has
/// its init in its own cgroup and the namespaces its spec asks for, and no `hex` binary was exec'd for
/// the create. Do: create and start a box whose init is a non-hex program (`/usr/bin/sleep`) with a
/// network namespace of its own; read the init's `/proc/<pid>/cgroup`, `ns/*` and `exe`. Expect: the
/// init is in the box's cgroup, in fresh pid, mount, ipc, uts and network namespaces (and a user
/// namespace rootless), its `exe` is the program (not the `hex` binary), the create was handed a seed and
/// done with it and none went through the shim (the pool's counts: the `hex __oci-init` re-exec the seed
/// replaces didn't run), and its streams are the null device, the seed's own.
fn created_through_a_seed_in_its_cgroup_and_namespaces(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let box_root = own.join("hex-oci-seedns");
    let node = lab.node(&box_root, state.path());
    let id = BoxId::new("nsbox").unwrap();
    // A non-hex init, with a network namespace of its own, so the exe is plainly the program and every
    // namespace the spec asks for can be read.
    let bundle = lab.bundle_with(&strings(&["/usr/bin/sleep", "600"]), true);
    let before = node.create_seeds().unwrap();
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
    .expect("create through a seed");
    // The inits the node then makes ahead for boxes of no owner go through its create seeds too (decision 8):
    // their refill is waited out, its queue full (or a make failed), and they are taken out of the counts.
    wait_for(
        &AHEAD,
        || {
            let ahead = node.cgroup_reuse().unwrap().ahead;
            ahead.ready >= ahead.size || ahead.failed > 0
        },
        "the inits made ahead to fill their queue, or a make to fail",
    );
    let parks = node.cgroup_reuse().unwrap().parked.parked;
    let after = node.create_seeds().unwrap();
    assert_eq!(
        (
            after.handed - before.handed - parks,
            after.done - before.done - parks,
            node.creates_through_shim()
        ),
        (1, 1, 0),
        "the create was handed a seed and done with it, and none went through the shim: {after:?}"
    );
    node.start(&id).expect("start");
    let init = u32::try_from(init_pid(&node.container_dir(&id))).unwrap();
    // Wait until the init has exec'd its program, so `exe`, the namespaces and the cgroup are the
    // container's and not the seed's clone mid-setup.
    wait_for(
        &PROGRAM_RUN,
        || std::fs::read_link(format!("/proc/{init}/exe")).is_ok_and(|p| p.ends_with("sleep")),
        "the init to exec its program",
    );

    let exe = std::fs::read_link(format!("/proc/{init}/exe")).unwrap();
    assert!(
        exe.ends_with("sleep") && !exe.to_string_lossy().contains("hex"),
        "the init is the program, not a hex shim: {}",
        exe.display()
    );
    // Its cgroup is the box's own (read in this process's cgroup namespace: the full path).
    let cgroup = std::fs::read_to_string(format!("/proc/{init}/cgroup")).unwrap();
    assert!(
        cgroup
            .lines()
            .any(|l| l.starts_with("0::") && l.ends_with("/nsbox")),
        "the init is in the box's cgroup: {cgroup:?}"
    );
    // Each namespace its spec makes is its own (differs from the agent's); a rootless box also has a
    // user namespace of its own.
    let agent = std::process::id();
    let mut kinds = vec!["pid", "mnt", "ipc", "uts", "net"];
    if lab.rootless {
        kinds.push("user");
    }
    for ns in kinds {
        let theirs = std::fs::read_link(format!("/proc/{init}/ns/{ns}")).unwrap();
        let ours = std::fs::read_link(format!("/proc/{agent}/ns/{ns}")).unwrap();
        assert_ne!(theirs, ours, "the init has its own {ns} namespace");
    }
    // It leads a session and a process group of its own (its first act), so nothing it or its program
    // does reaches the agent's: both ids are its own pid, kept across its exec.
    assert_eq!(
        (stat_field(init, 5), stat_field(init, 6)),
        (Some(init), Some(init)),
        "the init leads its own process group and session"
    );
    // The seed cloned the init as the agent's own child (`CLONE_PARENT`), and the init's streams are the
    // seed's: the null device, never the agent's.
    assert_eq!(ppid(init), Some(agent), "the init is the agent's own child");
    assert_eq!(
        streams(init),
        ["/dev/null"; 3].map(PathBuf::from),
        "the init has the null device for its streams"
    );
    node.delete(&id).expect("delete");
}

/// Goal 5 plan step 4 (T-T7.1.4), the re-initialization test: two containers cloned from one seed are
/// each randomized afresh, so neither carries the seed's layout (f15, f17: the child execs the
/// container's program, so its address space is randomized anew, not the seed's copied one). Do: create
/// two boxes in turn from the same idle seed and read each init's stack base, and the seed's, from
/// `/proc/<pid>/maps`. Expect: the two inits' stack bases differ, and each differs from the seed's — the
/// value each child "makes" (its ASLR'd layout) is its own.
fn each_seed_cloned_container_is_randomized_afresh(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    let node = lab.node(&own.join("hex-oci-reinit"), state.path());
    let agent = std::process::id();
    let base = |pid: u32| {
        wait_for(
            &CREATING,
            || stack_base(pid).is_some(),
            "a process to have a readable stack mapping",
        );
        stack_base(pid).unwrap()
    };
    let make = |id: &str| -> u32 {
        let bid = BoxId::new(id).unwrap();
        node.create(lab.running(bid.clone(), &strings(&["/usr/bin/sleep", "600"])))
            .expect("create");
        node.start(&bid).expect("start");
        let init = u32::try_from(init_pid(&node.container_dir(&bid))).unwrap();
        wait_for(
            &PROGRAM_RUN,
            || std::fs::read_link(format!("/proc/{init}/exe")).is_ok_and(|p| p.ends_with("sleep")),
            "the init to exec its program",
        );
        init
    };

    let one = make("reinit1");
    // The seed that cloned it, now idle: the agent's child running `hex __oci-seed`. The next create
    // reuses it, so both inits are cloned from this one seed.
    let seed = children_of(agent)
        .into_iter()
        .find(|&c| cmdline_has(c, "__oci-seed"))
        .expect("a create seed is up");
    let (v1, vs) = (base(one), base(seed));
    let two = make("reinit2");
    let v2 = base(two);
    assert_ne!(v1, v2, "two containers from one seed differ: {v1} vs {v2}");
    assert_ne!(v1, vs, "the first differs from the seed's: {v1} vs {vs}");
    assert_ne!(v2, vs, "the second differs from the seed's: {v2} vs {vs}");
    node.delete(&BoxId::new("reinit1").unwrap())
        .expect("delete");
    node.delete(&BoxId::new("reinit2").unwrap())
        .expect("delete");
}

/// Goal 5 plan step 4 (T-T7.1.4), crash injection: a seed that dies mid-create refuses that create by
/// type, its box is cleaned up, and the seed is restarted, counted, so the next create succeeds. Do:
/// create and delete a box, so the pool holds two idle seeds, its first (the one the next create is
/// handed: the core hands the lowest idle slot) and the spare it started beside it; stop the first; create
/// a second box on another thread and, once the pool has handed it the stopped seed, kill that seed; then
/// create a third. Expect: the second create is refused by type and left no box; the pool counts the
/// death and the create its death refused; the third create succeeds and the pool counts a restart.
fn a_seed_that_dies_mid_create_is_refused_and_restarted(lab: &Lab, own: &Path) {
    let state = tempfile::tempdir_in(lab.bundles.path()).unwrap();
    // The seed path: a node that keeps no cgroups made ahead, whose creates each go to a seed (decision 8's
    // dry path, which a node keeping them takes when none is ready).
    let node = lab
        .node(&own.join("hex-oci-seedcrash"), state.path())
        .with_inits_ahead(0);
    let agent = std::process::id();
    let first = BoxId::new("crash-a").unwrap();
    node.create(lab.running(first.clone(), &Lab::act(&Act::Wait)))
        .expect("the first create, through the pool's first seed");
    node.delete(&first)
        .expect("delete the first box, leaving its seed idle");
    // The pool's first seed, the one started earliest: it served the first create, and is in the lowest
    // slot, so the next create is handed it.
    let seed = children_of(agent)
        .into_iter()
        .filter(|&c| cmdline_has(c, "__oci-seed"))
        .min_by_key(|&c| start_ticks(c))
        .expect("a create seed is up");
    let seed = i32::try_from(seed).unwrap();
    kill(seed, 19); // SIGSTOP: it holds whatever create it is handed, unanswered
    let before = node.create_seeds().unwrap();

    let dead = BoxId::new("crash-b").unwrap();
    let refused = std::thread::scope(|scope| {
        let creating =
            scope.spawn(|| node.create(lab.running(dead.clone(), &Lab::act(&Act::Wait))));
        wait_for(
            &CREATING,
            || node.create_seeds().unwrap().handed > before.handed,
            "the second create to be handed the stopped seed",
        );
        // Mid-create: the seed holds the create, which waits on its answer. It dies.
        kill(seed, 9);
        creating.join().unwrap()
    });
    assert!(
        matches!(refused, Err(Refusal::Os { .. }) | Err(Refusal::Broken(_))),
        "a create whose seed dies is refused by type: {refused:?}"
    );
    assert_eq!(
        node.inspect(&dead),
        Err(Refusal::NoSuchBox(dead.clone())),
        "its box is cleaned up"
    );
    assert!(
        !own.join("hex-oci-seedcrash")
            .join(OCI_BOXES)
            .join("crash-b")
            .exists()
    );
    let counts = node.create_seeds().unwrap();
    assert_eq!(
        (
            counts.died - before.died,
            counts.refused_died - before.refused_died
        ),
        (1, 1),
        "the seed's death and the create it refused are counted: {counts:?}"
    );

    // The next create is handed a live seed, and the dead one's slot is started again, counted.
    let next = BoxId::new("crash-c").unwrap();
    node.create(lab.running(next.clone(), &Lab::act(&Act::Wait)))
        .expect("the next create succeeds");
    node.start(&next).expect("start the next box");
    assert_eq!(node.inspect(&next).unwrap().state, BoxState::Running);
    wait_for(
        &SEED_RESTARTED,
        || node.create_seeds().unwrap().restarted > before.restarted,
        "the dead seed's slot to be started again",
    );
    node.delete(&next).expect("delete");
}

/// The start address of `pid`'s main-thread stack, from `/proc/<pid>/maps` (`[stack]`), as a string;
/// `None` while it can't be read. Randomized afresh at each `execve`, so it tells one container's
/// address space from another's and from the seed's.
fn stack_base(pid: u32) -> Option<String> {
    let maps = std::fs::read_to_string(format!("/proc/{pid}/maps")).ok()?;
    maps.lines()
        .find(|l| l.ends_with("[stack]"))
        .and_then(|l| l.split('-').next())
        .map(str::to_owned)
}

/// What `pid`'s standard streams are open on: the targets of `/proc/<pid>/fd/0`, `1` and `2`.
fn streams<P: std::fmt::Display>(pid: P) -> [PathBuf; 3] {
    [0, 1, 2].map(|fd| std::fs::read_link(format!("/proc/{pid}/fd/{fd}")).unwrap())
}

/// When `pid` started, in clock ticks after boot (`/proc/<pid>/stat` field 22).
fn start_ticks(pid: u32) -> u64 {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    let (_, rest) = stat.rsplit_once(')').unwrap();
    rest.split_whitespace().nth(19).unwrap().parse().unwrap()
}

fn sorted_ms(latencies: &[Duration]) -> Vec<Duration> {
    let mut sorted = latencies.to_vec();
    sorted.sort_unstable();
    sorted
}

/// Whether a thread of this process is blocked opening a FIFO no one has opened the other end of: the
/// kernel shows it waiting in `wait_for_partner` (fs/pipe.c), or in `fifo_open` where that's inlined.
/// Lets a create held reading its bundle's config.json, a FIFO, go with `text`, as [`swap_the_held_config`] does
/// with the same text for whatever reads the config after the agent.
fn let_the_held_create_go(config: &Path, text: &str) {
    swap_the_held_config(config, text, text);
}

/// Lets a create held reading its bundle's config.json, a FIFO, go: once the agent has the FIFO open to read, a
/// file of `later` takes its name, for whatever opens config.json after the agent's read, and the agent's own
/// read gets `agent`. Since the seed and the shim take the spec the runtime checked, nothing of a create reads the
/// file after the agent (a_config_swapped_after_the_check_never_makes_the_container holds it so); before, the
/// seed's open of the FIFO alone waited for a writer and the create ended in the seed's patience.
fn swap_the_held_config(config: &Path, agent: &str, later: &str) {
    let mut fifo = std::fs::OpenOptions::new()
        .write(true)
        .open(config)
        .unwrap();
    let file = config.with_extension("file");
    std::fs::write(&file, later).unwrap();
    std::fs::rename(&file, config).unwrap();
    std::io::Write::write_all(&mut fifo, agent.as_bytes()).unwrap();
}

/// Whether process `pid` holds CAP_SYS_ADMIN (capability 21) in its effective set, as /proc/<pid>/status says.
fn holds_sys_admin(pid: u32) -> bool {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).unwrap();
    let eff = status
        .lines()
        .find_map(|l| l.strip_prefix("CapEff:"))
        .expect("a CapEff line");
    u64::from_str_radix(eff.trim(), 16).unwrap() & (1 << 21) != 0
}

fn blocked_opening_a_fifo() -> bool {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(Result::ok)
        .filter_map(|task| std::fs::read_to_string(task.path().join("wchan")).ok())
        .any(|wchan| matches!(wchan.trim(), "wait_for_partner" | "fifo_open"))
}

/// Whether `dir` lies on tmpfs (include/uapi/linux/magic.h, `TMPFS_MAGIC`).
fn on_tmpfs(dir: &Path) -> bool {
    rustix::fs::statfs(dir).is_ok_and(|fs| i128::from(fs.f_type) == 0x0102_1994)
}

/// Polls inspect until the box reports an exit: the fact the test needs, bounded so a runtime that
/// never reports one fails rather than hangs.
fn wait_exit(rt: &dyn Runtime, id: &BoxId) -> BoxInfo {
    let mut last = None;
    wait_for(
        &PROGRAM_RUN,
        || {
            let info = rt.inspect(id).unwrap();
            let done = info.exit.is_some();
            last = Some(info);
            done
        },
        "the box to report its init's exit",
    );
    last.unwrap()
}

/// Waits for `cond` on `wait`, its own fact's record or measuring (hex_testkit::waits): says how many looks it took.
fn wait_for(wait: &Patience, cond: impl FnMut() -> bool, what: &str) -> u64 {
    until(wait, what, cond).unwrap()
}

/// The init's pid, from the state (`"pid":<n>`) in its container's state dir.
fn init_pid(container: &Path) -> i32 {
    let text = std::fs::read_to_string(container.join("state.json")).unwrap();
    let (_, after) = text.split_once("\"pid\":").unwrap();
    after
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap()
        .parse()
        .unwrap()
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_owned()).collect()
}

/// Field 4 of `/proc/<pid>/stat`; 0 once it's gone.
fn parent_of(pid: i32) -> i32 {
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|s| {
            s.rsplit_once(')')
                .and_then(|(_, rest)| rest.split_whitespace().nth(1).map(str::to_owned))
        })
        .and_then(|p| p.parse().ok())
        .unwrap_or(0)
}

fn kill(pid: i32, sig: i32) {
    let status = Command::new("kill")
        .args([format!("-{sig}"), pid.to_string()])
        .status()
        .unwrap();
    assert!(status.success(), "kill -{sig} {pid}");
}

/// `args` as a JSON array: Debug quoting is JSON's for the plain ASCII the tests use.
fn json_args(args: &[String]) -> String {
    assert!(args.iter().all(|a| a.is_ascii() && !a.contains('\\')));
    format!("{args:?}")
}

/// The capability sets the node renders a bundle with (Docker's defaults, `hex_core::bundle`). A spec
/// naming none keeps its creator's, CAP_SYS_ADMIN among them, which a rootless box on the host's
/// network may not hold (spec.rs refuses it).
fn capabilities() -> String {
    let set = format!("{:?}", hex_core::bundle::CAPABILITIES);
    format!(r#"{{"bounding": {set}, "effective": {set}, "permitted": {set}}}"#)
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
    let own = std::fs::read_to_string("/proc/self/cgroup").expect("/proc/self/cgroup");
    let rel = own
        .lines()
        .find_map(|l| l.strip_prefix("0::"))
        .expect("a 0:: cgroup line")
        .trim_start_matches('/');
    PathBuf::from("/sys/fs/cgroup").join(rel)
}

/// Moves this process into a leaf of its own cgroup and confirms every controller is delegated, as
/// oci_seccomp.rs does; returns the own cgroup dir, or why it isn't ours to subdivide.
fn prepare_cgroup(disk: &Path) -> Result<PathBuf, String> {
    let own = LAB_PARENT.get_or_init(own_cgroup).clone();
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
