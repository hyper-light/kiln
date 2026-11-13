//! AC-T7.1.4 (runtime-tools linux_mount_label, linux_process_apparmor_profile) and CLAUDE.md §3: the `oci` class
//! applies `linux.mountLabel` to every filesystem its mounts make and `process.apparmorProfile` to its process;
//! where the host's kernel doesn't run the module, it skips the label with a warning, or refuses it by name under
//! `HEX_OCI_LSM=strict` (docs/design/runtime.md, "SELinux mount
//! labels and AppArmor profiles"). "do X, expect Y" (§6):
//! - Create a container with a mount label. Where SELinux is active, expect its `/tmp` and `/dev` tmpfs and a
//!   masked directory's to read that label. Where it isn't, expect it to run unlabelled by default, as crun
//!   runs it, and under the strict policy expect the create refused, naming `linux.mountLabel`.
//! - Create a container with an AppArmor profile. Where AppArmor is on and `HEX_LSM_APPARMOR_PROFILE` names a
//!   loaded profile, expect the container's first process to run under it. Where AppArmor is off, expect it to
//!   run without the profile by default, and under the strict policy expect the create refused, naming
//!   `process.apparmorProfile`.
//!
//! Rootless or as root, with a delegated cgroup v2 subtree, as oci_seccomp.rs runs. Where neither is possible,
//! or AppArmor is on but no profile is named, it prints a reason and returns rather than pass falsely.
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
use hex_runtime::linux::oci::lsm::{self, Policy};

/// The label the test mounts with: Fedora's container file type, at the categories runtime-tools uses.
const DEFAULT_LABEL: &str = "system_u:object_r:container_file_t:s0:c715,c811";

/// The probe for the mount label: exit 42 when `/tmp`, `/dev` and the masked `/masked`'s tmpfs read `$L`,
/// 10 when one doesn't.
const LABEL_PROBE: &str = r#"for p in /tmp /dev /masked; do [ "$(stat -c %C "$p")" = "$L" ] || { stat -c "%n %C" "$p"; exit 10; }; done; exit 42"#;

/// The probe for the profile: exit 42 when the container's first process runs under `$P`, 10 when not.
const PROFILE_PROBE: &str = r#"c=$(cat /proc/1/attr/apparmor/current 2>/dev/null || cat /proc/1/attr/current); case "$c" in "$P "*|"$P") exit 42;; *) echo "$c"; exit 10;; esac"#;

#[test]
fn oci_mount_label_is_applied_or_refused_by_name() {
    in_delegated_scope(
        "oci_mount_label_is_applied_or_refused_by_name",
        |own, cache, exe| {
            let label =
                std::env::var("HEX_LSM_MOUNT_LABEL").unwrap_or_else(|_| DEFAULT_LABEL.to_owned());
            let field = format!(r#","mountLabel":"{label}""#);
            if !control_runs(own, cache, exe) {
                return;
            }
            let outcome = run_probe(
                own,
                cache,
                exe,
                "lsm-label",
                &field,
                "",
                LABEL_PROBE,
                ("L", &label),
                Policy::Skip,
            );
            if selinux_active() {
                match outcome {
                    Ok(code) => assert_eq!(
                        code,
                        Exit::Code(42),
                        "every tmpfs should read the label {label}"
                    ),
                    Err(e) => panic!("the labelled container failed where SELinux is active: {e}"),
                }
            } else {
                // As crun does where the module is off: the container runs, unlabelled, and the create
                // warns, naming the label (the lead's goal-17 bar, docs/design/runtime.md).
                assert_eq!(
                    outcome.expect(
                        "SELinux isn't active, so by default the label is skipped with a warning"
                    ),
                    Exit::Code(10),
                    "the container runs, its tmpfs unlabelled"
                );
                let strict = run_probe(
                    own,
                    cache,
                    exe,
                    "lsm-label-strict",
                    &field,
                    "",
                    LABEL_PROBE,
                    ("L", &label),
                    Policy::Strict,
                )
                .expect_err("under HEX_OCI_LSM=strict the label is refused, not skipped");
                assert!(
                    strict.contains("linux.mountLabel") && strict.contains("strict"),
                    "the refusal should name linux.mountLabel and the strict policy: {strict}"
                );
            }
        },
    );
}

#[test]
fn oci_apparmor_profile_is_applied_or_refused_by_name() {
    in_delegated_scope(
        "oci_apparmor_profile_is_applied_or_refused_by_name",
        |own, cache, exe| {
            // The kernel asked as a create asks it, not /sys, which a privileged container may not show.
            let on = lsm::apparmor_runs().expect("asking the kernel whether AppArmor runs");
            let profile = std::env::var("HEX_LSM_APPARMOR_PROFILE").ok();
            let name = match (&profile, on) {
                (Some(p), true) => p.clone(),
                (None, true) => {
                    eprintln!(
                        "SKIP: AppArmor is on but HEX_LSM_APPARMOR_PROFILE names no loaded profile to apply"
                    );
                    return;
                }
                (_, false) => "hex-test-profile".to_owned(),
            };
            let field = format!(r#","apparmorProfile":"{name}""#);
            if !control_runs(own, cache, exe) {
                return;
            }
            let outcome = run_probe(
                own,
                cache,
                exe,
                "lsm-profile",
                "",
                &field,
                PROFILE_PROBE,
                ("P", &name),
                Policy::Skip,
            );
            if on {
                match outcome {
                    Ok(code) => assert_eq!(
                        code,
                        Exit::Code(42),
                        "the first process should run under {name}"
                    ),
                    Err(e) => panic!("the profiled container failed where AppArmor is on: {e}"),
                }
            } else {
                // As crun does where the module is off: the container runs without the profile, and the
                // create warns, naming it.
                assert_eq!(
                    outcome.expect(
                        "AppArmor is off, so by default the profile is skipped with a warning"
                    ),
                    Exit::Code(10),
                    "the container runs, not under the profile"
                );
                let strict = run_probe(
                    own,
                    cache,
                    exe,
                    "lsm-profile-strict",
                    "",
                    &field,
                    PROFILE_PROBE,
                    ("P", &name),
                    Policy::Strict,
                )
                .expect_err("under HEX_OCI_LSM=strict the profile is refused, not skipped");
                assert!(
                    strict.contains("process.apparmorProfile") && strict.contains("strict"),
                    "the refusal should name process.apparmorProfile and the strict policy: {strict}"
                );
            }
        },
    );
}

/// Runs the same container with neither a label nor a profile, the control: true where it ran, so a failure of
/// the labelled or profiled one is the label's or the profile's; false, with the reason printed, where this
/// host can't run the container at all (an unprivileged user namespace refused), which the test then skips.
fn control_runs(own: &Path, cache: &Path, exe: &str) -> bool {
    match run_probe(
        own,
        cache,
        exe,
        "lsm-control",
        "",
        "",
        "exit 42",
        ("C", ""),
        Policy::Skip,
    ) {
        Ok(code) => {
            assert_eq!(
                code,
                Exit::Code(42),
                "the control container should run its probe"
            );
            true
        }
        Err(e) if is_userns_refusal(&e) => {
            eprintln!("SKIP: rootless OCI unavailable here ({e})");
            false
        }
        Err(e) => panic!("the control container (no label, no profile) failed: {e}"),
    }
}

/// Whether SELinux is active, asked as a create asks the kernel (its seclabel option), not by selinuxfs: a
/// privileged container on an SELinux host, the root lab's, has none mounted, though its kernel runs SELinux.
fn selinux_active() -> bool {
    lsm::selinux_runs().expect("asking the kernel whether SELinux runs")
}

/// Runs `body` with a cgroup of this process's own to subdivide, re-running the test `name` in a transient
/// systemd --user scope with `Delegate=yes` where this process's cgroup isn't one (oci_seccomp.rs's way).
fn in_delegated_scope(name: &str, body: impl FnOnce(&Path, &Path, &str)) {
    let exe = env!("CARGO_BIN_EXE_hex").to_owned();
    let cache = cache_dir();
    match prepare_cgroup(&cache) {
        Ok(own) => body(&own, &cache, &exe),
        Err(direct) if std::env::var_os("HEX_DELEGATED").is_none() => {
            rerun_delegated(name, &direct)
        }
        Err(direct) => {
            eprintln!("SKIP: no usable cgroup v2 delegation ({direct}); run on a cgroup-v2 host")
        }
    }
}

/// Creates a container with `linux_field` in its `linux` and `process_field` in its `process`, starts it, runs
/// `probe` through `hex oci exec` with `env` set, and deletes it. Returns the probe's exit, or why the create,
/// start or exec failed.
#[allow(clippy::too_many_arguments)] // a test helper; each argument is a distinct knob of the run
fn run_probe(
    own: &Path,
    cache: &Path,
    exe: &str,
    name: &str,
    linux_field: &str,
    process_field: &str,
    probe: &str,
    env: (&str, &str),
    policy: Policy,
) -> Result<Exit, String> {
    let (uid, gid) = (host_id("-u"), host_id("-g"));
    let bundle = tempfile::Builder::new()
        .prefix(&format!("bundle-{name}-"))
        .tempdir_in(cache)
        .map_err(|e| format!("bundle dir: {e}"))?;
    build_bundle(
        bundle.path(),
        uid,
        gid,
        uid != 0,
        linux_field,
        process_field,
    )
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
    .with_oci_helper(PathBuf::from(exe))
    .with_lsm(policy);
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
        let script = format!("{}='{}'; {probe}", env.0, env.1);
        let mut proc = node
            .exec(&id, Program::of(&["/bin/sh", "-c", &script]), Stdio::null())
            .map_err(|e| format!("exec: {e}"))?;
        proc.wait_for(Duration::from_secs(30))
            .map_err(|e| format!("wait: {e}"))?
            .ok_or_else(|| "the probe did not finish in 30s".to_owned())
    })();
    let _ = node.delete(&id);
    outcome
}

/// A minimal bundle as oci_seccomp.rs builds it: a rootfs of the host's `/usr` bound read-only, tmpfs for `/dev`
/// and `/tmp`, the directory `/masked` masked, and a user namespace mapping the caller to 0 where it isn't root.
fn build_bundle(
    dir: &Path,
    uid: u32,
    gid: u32,
    rootless: bool,
    linux_field: &str,
    process_field: &str,
) -> std::io::Result<()> {
    let rootfs = dir.join("rootfs");
    for d in ["proc", "dev", "tmp", "usr", "masked"] {
        std::fs::create_dir_all(rootfs.join(d))?;
    }
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
    "noNewPrivileges": true{process_field}
  }},
  "mounts": [
    {{"destination": "/proc", "type": "proc", "source": "proc"}},
    {{"destination": "/dev", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid","mode=755","size=65536k"]}},
    {{"destination": "/tmp", "type": "tmpfs", "source": "tmpfs", "options": ["nosuid","nodev"]}},
    {{"destination": "/usr", "type": "bind", "source": "/usr", "options": ["rbind","ro"]}}
  ],
  "linux": {{
    "namespaces": [{namespaces}]{mappings},
    "maskedPaths": ["/masked"]{linux_field}
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
        PathBuf::from(std::env::var_os("HOME").expect("HOME")).join(".cache/hex-t7/lsmtest");
    std::fs::create_dir_all(&cache).expect("cache dir");
    cache
}
