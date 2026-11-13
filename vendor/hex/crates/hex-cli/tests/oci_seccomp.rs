//! AC-T7.1.4 and CLAUDE.md §3: the `oci` class compiles a container's `linux.seccomp` profile and
//! loads it, so a syscall the profile refuses is refused inside the container while every other one
//! runs. "do X, expect Y" (§6): run the same probe — `mkdir` (refused by the profile) then `touch`
//! (allowed) — in a container whose profile refuses only `mkdir`, and in one with no profile; expect
//! the probe to report `mkdir` blocked and `touch` working under the profile, and `mkdir` working
//! without it. The difference is the filter: nothing else changes.
//!
//! Rootless: the container maps the caller to uid 0 in a user namespace, so this needs neither root
//! nor a privileged container, only unprivileged user namespaces and a delegated cgroup v2 subtree —
//! the same subtree process_box.rs needs, obtained the same way (a transient systemd --user scope
//! with `Delegate=yes`). Where that isn't possible it prints a reason and returns rather than pass
//! falsely. Exec (`hex oci exec`) runs the probe; init and exec share the code that loads the filter
//! (`init::become_process`), so this exercises that path end to end on a real kernel.
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
use std::time::Duration;

use hex_api::{Bounds, BoxId, BoxSpec, ClassSpec, Exit, Program, Runtime, Stdio};
use hex_node::Node;

/// The probe the container runs: `mkdir` (the profile refuses it) then `touch` (allowed). Its exit
/// code says which syscall did what, so the outcome is a fact the test waits on, not a guess (§4):
///   10 — `mkdir` succeeded (no filter, or it didn't bind)
///   11 — `touch` was refused (the filter was too broad)
///   42 — `mkdir` refused and `touch` allowed (the filter bound exactly the one syscall)
const PROBE: &str = "mkdir /tmp/seccomp-probe 2>/dev/null && exit 10; \
                     touch /tmp/seccomp-probe-file 2>/dev/null || exit 11; exit 42";

/// A profile that allows everything but `mkdir`/`mkdirat`, which it refuses with EPERM. No
/// `architectures`: the native architecture only (the compiler skips `mkdir`, absent on aarch64).
const DENY_MKDIR: &str = r#","seccomp":{"defaultAction":"SCMP_ACT_ALLOW",
    "syscalls":[{"names":["mkdir","mkdirat"],"action":"SCMP_ACT_ERRNO","errnoRet":1}]}"#;

#[test]
fn oci_seccomp_refuses_a_denied_syscall_and_allows_the_rest() {
    let exe = env!("CARGO_BIN_EXE_hex").to_owned();
    let cache = cache_dir();
    match prepare_cgroup(&cache) {
        Ok(own) => exercise(&own, &cache, &exe),
        Err(direct) if std::env::var_os("HEX_DELEGATED").is_none() => rerun_delegated(
            "oci_seccomp_refuses_a_denied_syscall_and_allows_the_rest",
            &direct,
        ),
        Err(direct) => {
            hex_testkit::skip(
                "oci_seccomp_refuses_a_denied_syscall_and_allows_the_rest",
                "cgroup v2 delegation",
                &format!("none in the delegated scope either ({direct}); run on a cgroup-v2 host"),
            )
            .expect("recording the skip");
        }
    }
}

/// The two runs: the probe under the deny-mkdir profile reports 42 (mkdir blocked, touch allowed);
/// the same probe with no profile reports 10 (mkdir works). If unprivileged user namespaces aren't
/// available the container can't be created rootless, and the run prints a reason and returns.
fn exercise(own: &Path, cache: &Path, exe: &str) {
    let (uid, gid) = (host_id("-u"), host_id("-g"));
    // Root (a privileged container, e.g. the fedora bench host) runs the container without a user
    // namespace; a non-root caller runs it rootless, mapping itself to 0. Either way the seccomp
    // profile is the same and loaded the same way.
    let rootless = uid != 0;

    let with = run_probe(own, cache, exe, "sec-on", uid, gid, rootless, true);
    match with {
        Ok(code) => assert_eq!(
            code,
            Exit::Code(42),
            "under the deny-mkdir profile the probe should report mkdir blocked + touch allowed (42)"
        ),
        Err(e) if is_userns_refusal(&e) => {
            hex_testkit::skip(
                "oci_seccomp_refuses_a_denied_syscall_and_allows_the_rest",
                "unprivileged user namespaces",
                &format!("rootless OCI unavailable here ({e})"),
            )
            .expect("recording the skip");
            return;
        }
        Err(e) => panic!("the profiled container failed: {e}"),
    }

    let without = run_probe(own, cache, exe, "sec-off", uid, gid, rootless, false)
        .expect("the control container (no profile) should run");
    assert_eq!(
        without,
        Exit::Code(10),
        "with no profile the probe should make the directory (10): the control that shows the filter is the cause"
    );
}

/// Creates a rootless container that sleeps, starts it, runs the probe through `hex oci exec`, and
/// deletes it. Returns the probe's exit.
#[allow(clippy::too_many_arguments)] // a test helper; each argument is a distinct knob of the run
fn run_probe(
    own: &Path,
    cache: &Path,
    exe: &str,
    name: &str,
    uid: u32,
    gid: u32,
    rootless: bool,
    seccomp: bool,
) -> Result<Exit, String> {
    let bundle = tempfile::Builder::new()
        .prefix(&format!("bundle-{name}-"))
        .tempdir_in(cache)
        .map_err(|e| format!("bundle dir: {e}"))?;
    build_bundle(bundle.path(), uid, gid, rootless, seccomp)
        .map_err(|e| format!("building the bundle: {e}"))?;

    let state = tempfile::Builder::new()
        .prefix(&format!("state-{name}-"))
        .tempdir_in(cache)
        .map_err(|e| format!("state dir: {e}"))?;
    let node = Node::open(
        &own.join(format!("hex-oci-{name}")),
        state.path(),
        state.path(),
    )
    .map_err(|e| format!("open node: {e}"))?
    .with_oci_helper(PathBuf::from(exe));

    let id = BoxId::new(name).map_err(|e| e.to_string())?;
    let spec = BoxSpec {
        route: None,
        allocation: None,
        devices: Vec::new(),
        id: id.clone(),
        bounds: Bounds::default(),
        class: ClassSpec::Bundle(bundle.path().to_path_buf()),
        owner: None,
        band: 0,
    };
    node.create(spec).map_err(|e| e.to_string())?;
    // delete runs whatever happens, so no container is left behind.
    let outcome = (|| -> Result<Exit, String> {
        node.start(&id).map_err(|e| format!("start: {e}"))?;
        let mut proc = node
            .exec(&id, Program::of(&["/bin/sh", "-c", PROBE]), Stdio::null())
            .map_err(|e| format!("exec: {e}"))?;
        proc.wait_for(Duration::from_secs(30))
            .map_err(|e| format!("wait: {e}"))?
            .ok_or_else(|| "the probe did not finish in 30s".to_owned())
    })();
    let _ = node.delete(&id);
    outcome
}

/// A minimal rootless bundle: a rootfs of the host's `/usr` (usr-merged, so `/bin`→`/usr/bin`, etc.)
/// bound read-only, tmpfs for `/dev` and `/tmp`, a user namespace mapping the caller to 0, and the
/// `sleep` the probe execs beside.
fn build_bundle(
    dir: &Path,
    uid: u32,
    gid: u32,
    rootless: bool,
    seccomp: bool,
) -> std::io::Result<()> {
    let rootfs = dir.join("rootfs");
    for d in ["proc", "dev", "tmp", "usr"] {
        std::fs::create_dir_all(rootfs.join(d))?;
    }
    // usr-merge: mirror the host's own top-level links, so /bin/sh and the program's loader resolve
    // into the bound /usr as they do on the host. They differ by distribution: Arch links lib64 to
    // usr/lib and sbin to usr/bin, Fedora lib64 to usr/lib64 (its loader is there alone) and sbin to
    // usr/sbin. A host whose top level isn't linked into /usr can't run this rootfs of /usr alone.
    for link in ["bin", "sbin", "lib", "lib64"] {
        let at = rootfs.join(link);
        if !at.exists() {
            let target = std::fs::read_link(Path::new("/").join(link)).map_err(|e| {
                std::io::Error::other(format!(
                    "the host's /{link} isn't a link into /usr ({e}), so a rootfs of /usr alone can't run its programs"
                ))
            })?;
            std::os::unix::fs::symlink(target, at)?;
        }
    }
    let seccomp_field = if seccomp { DENY_MKDIR } else { "" };
    // The capability sets the node renders a bundle with (Docker's defaults, `hex_core::bundle`). A
    // spec naming none keeps its creator's, CAP_SYS_ADMIN among them, which a rootless box on the
    // host's network may not hold (spec.rs refuses it).
    let set = format!("{:?}", hex_core::bundle::CAPABILITIES);
    // Rootless needs a user namespace and the caller-to-0 maps; root runs without them.
    let (namespaces, mappings) = if rootless {
        (
            r#"{"type":"user"},{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"}"#
                .to_owned(),
            format!(
                ",\n    \"uidMappings\": [{{\"containerID\":0,\"hostID\":{uid},\"size\":1}}],\n    \"gidMappings\": [{{\"containerID\":0,\"hostID\":{gid},\"size\":1}}]"
            ),
        )
    } else {
        (
            r#"{"type":"pid"},{"type":"mount"},{"type":"ipc"},{"type":"uts"}"#.to_owned(),
            String::new(),
        )
    };
    let config = format!(
        r#"{{
  "ociVersion": "1.3.0",
  "root": {{"path": "rootfs", "readonly": false}},
  "process": {{
    "cwd": "/",
    "args": ["/bin/sh", "-c", "sleep 1000000"],
    "env": ["PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"],
    "user": {{"uid": 0, "gid": 0}},
    "capabilities": {{"bounding": {set}, "effective": {set}, "permitted": {set}}},
    "noNewPrivileges": true
  }},
  "mounts": [
    {{"destination": "/proc", "type": "proc", "source": "proc"}},
    {{"destination": "/dev", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid","mode=755","size=65536k"]}},
    {{"destination": "/tmp", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid","nodev"]}},
    {{"destination": "/usr", "type": "bind", "source": "/usr", "options": ["rbind","ro"]}}
  ],
  "linux": {{
    "namespaces": [{namespaces}]{mappings}{seccomp_field}
  }}
}}"#
    );
    std::fs::write(dir.join("config.json"), config)
}

/// How long a delegated re-run may take: it runs this test's few containers once more, each wait of theirs
/// already bounded (run_probe's 30 s, conformance's PATIENCE), so past this it is stuck, not slow.
const RERUN_BOUND: Duration = Duration::from_secs(300);

/// Re-runs the test `name` in a transient systemd --user scope with `Delegate=yes`, so it gets a cgroup of its
/// own to subdivide. Only where a user manager answers (its `$XDG_RUNTIME_DIR/systemd/private` socket is
/// there): a `systemd-run` binary with no manager running, as in a guest whose PID 1 isn't systemd, would wait
/// on a manager that never comes. Prints the reason and returns where there's none. The re-run is waited on
/// for at most [`RERUN_BOUND`], then killed and failed by name.
fn rerun_delegated(name: &str, direct: &str) {
    let manager = std::env::var_os("XDG_RUNTIME_DIR")
        .map(|d| Path::new(&d).join("systemd/private"))
        .is_some_and(|p| p.exists());
    if !manager {
        eprintln!(
            "SKIP: own cgroup isn't ours to subdivide ({direct}) and no systemd --user manager answers for a private scope"
        );
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
    // for at most RERUN_BOUND, and a hung re-run is killed by its pid.
    let pid = child.id();
    let (sent, ended) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sent.send(child.wait());
    });
    let status = match ended.recv_timeout(RERUN_BOUND) {
        Ok(status) => status.expect("wait on the delegated re-run"),
        Err(_) => {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
            panic!(
                "the delegated re-run of {name} was still running after {RERUN_BOUND:?}, so it was killed"
            );
        }
    };
    assert!(status.success(), "the delegated re-run failed");
}

/// Whether a failure is the kernel refusing an unprivileged user namespace or map (so the run
/// should skip, not fail): the host, not Hex, lacks the capability.
fn is_userns_refusal(e: &str) -> bool {
    let e = e.to_lowercase();
    e.contains("userns")
        || e.contains("user namespace")
        || e.contains("uid_map")
        || e.contains("gid_map")
        || e.contains("clone")
        || e.contains("eperm")
        || e.contains("operation not permitted")
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
/// process_box.rs does; returns the own cgroup dir, or why it isn't ours to subdivide.
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
        PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/hex-t7/secctest");
    std::fs::create_dir_all(&cache).expect("cache dir");
    cache
}
