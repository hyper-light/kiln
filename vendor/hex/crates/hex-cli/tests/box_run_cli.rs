//! `hex box run` at the command line (AC-T7.1.1/AC-T7.1.2): it runs a program in a `process` box,
//! passes the program's exit status through, and a program over the box's memory bound is killed.
//!
//! A `process` box needs a cgroup v2 subtree the wrapper owns alone (its own cgroup, with the
//! controllers delegated). Where HEX_DELEGATED says this test binary runs alone in a cgroup delegated
//! to it (as a runner holding a whole suite in one capped scope places each test binary), each
//! `hex box run` starts in a fresh child of that cgroup, joined at exec, so the wrapper, its box and
//! its program stay under the cgroup's caps. Otherwise each is launched in its own transient
//! `systemd-run --user --scope -p Delegate=yes`, exactly how a kiln runner (with `Delegate=yes` on its
//! unit) invokes it. Where neither is available the test records the skip (hex_testkit, which the
//! gate prints) rather than pass falsely. Run on a cgroup-v2 host.
#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::disallowed_macros
)]

use std::process::Command;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use hex_api::{Bounds, BoxId};
use hex_runtime::linux::cgroup::BoxRoot;

const HEX: &str = env!("CARGO_BIN_EXE_hex");

/// How long a run's child cgroup may take to read empty once its wrapper is reaped (it stays a member
/// until the kernel's last switch away from it, box_run.rs): `hex box run`'s own CLEANUP_PATIENCE.
const EMPTY_PATIENCE: Duration = Duration::from_secs(10);

fn have_systemd() -> bool {
    Command::new("systemd-run")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A process's cgroup v2 path, from its `/proc/<pid>/cgroup` text.
fn cgroup_of(text: &str) -> String {
    text.lines()
        .find_map(|l| l.strip_prefix("0::"))
        .expect("a 0:: cgroup line")
        .to_owned()
}

/// The cgroup this test binary started in, read once, before anything here moves it.
fn started_in() -> &'static str {
    static STARTED: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    STARTED.get_or_init(|| {
        cgroup_of(&std::fs::read_to_string("/proc/self/cgroup").expect("reading /proc/self/cgroup"))
    })
}

/// Where each `hex box run` here starts.
enum Scope {
    /// A fresh child of this binary's own delegated cgroup per run, under this root.
    Own(BoxRoot),
    /// A transient `systemd-run --user --scope -p Delegate=yes` per run.
    Systemd,
}

/// Decided once: under HEX_DELEGATED, this binary's own delegated cgroup (a scope beside it would
/// escape the runner's caps); otherwise systemd's scopes; else why neither is there.
fn scope() -> Result<&'static Scope, &'static str> {
    static SCOPE: OnceLock<Result<Scope, String>> = OnceLock::new();
    SCOPE
        .get_or_init(|| {
            if std::env::var_os("HEX_DELEGATED").is_some() {
                own_root().map(Scope::Own)
            } else if have_systemd() {
                Ok(Scope::Systemd)
            } else {
                Err("no systemd --user to run a delegated scope".to_owned())
            }
        })
        .as_ref()
        .map_err(String::as_str)
}

/// The box root under this binary's own cgroup, as the process-box tests make theirs: this process
/// moves into a leaf of the cgroup, and the root beside that leaf opens only with every controller a
/// box uses handed down (`BoxRoot::open` refuses by name). `Err` when the cgroup isn't this binary's
/// alone to subdivide.
fn own_root() -> Result<BoxRoot, String> {
    let own = std::path::Path::new("/sys/fs/cgroup").join(started_in().trim_start_matches('/'));
    let leaf = own.join("hex-agent");
    std::fs::create_dir_all(&leaf).map_err(|e| format!("making a leaf for the test: {e}"))?;
    std::fs::write(leaf.join("cgroup.procs"), std::process::id().to_string())
        .map_err(|e| format!("moving the test into its leaf: {e}"))?;
    BoxRoot::open(&own.join("box-run"), Vec::new()).map_err(|e| e.to_string())
}

/// Runs `program args…` where this binary's `hex box run`s start, capturing its output.
fn run_in_scope(program: &str, args: &[&str]) -> std::process::Output {
    match scope().expect("a delegated cgroup to run hex box run in") {
        Scope::Systemd => Command::new("systemd-run")
            .args([
                "--user",
                "--scope",
                "--quiet",
                "-p",
                "Delegate=yes",
                "--",
                program,
            ])
            .args(args)
            .output()
            .expect("run in a transient scope"),
        Scope::Own(root) => {
            static RUNS: AtomicU64 = AtomicU64::new(0);
            let id = BoxId::new(&format!("run-{}", RUNS.fetch_add(1, Ordering::Relaxed))).unwrap();
            let child = root
                .create(&id, &Bounds::default())
                .expect("a child of this binary's cgroup");
            let mut cmd = Command::new(program);
            cmd.args(args);
            hex_runtime::linux::process::join_on_exec(&child, &mut cmd)
                .expect("joining the child at exec");
            let out = cmd
                .output()
                .expect("run in a child of this binary's cgroup");
            assert!(
                child
                    .wait_empty(EMPTY_PATIENCE)
                    .expect("waiting on the child"),
                "{} still holds a process {EMPTY_PATIENCE:?} after its wrapper ended",
                child.dir().display()
            );
            child.remove().expect("removing the child");
            out
        }
    }
}

/// Runs `hex box run <limits> -- program…` where this binary's runs start, capturing its output.
fn box_run(limits: &[&str], program: &[&str]) -> std::process::Output {
    let mut args = vec!["box", "run"];
    args.extend_from_slice(limits);
    args.push("--");
    args.extend_from_slice(program);
    run_in_scope(HEX, &args)
}

#[test]
fn box_run_boxes_a_program_and_passes_its_status() {
    if let Err(why) = scope() {
        hex_testkit::skip(
            "box_run_boxes_a_program_and_passes_its_status",
            "a delegated cgroup or systemd --user",
            &format!("no delegated cgroup to run hex box run in ({why}); run on a cgroup-v2 host"),
        )
        .expect("recording the skip");
        return;
    }

    // Its stdout passes through and its exit status is the program's.
    let out = box_run(
        &[
            "--memory-max",
            "268435456",
            "--cpu-weight",
            "50",
            "--io-weight",
            "10",
        ],
        &["/bin/sh", "-c", "echo boxed; exit 7"],
    );
    assert_eq!(out.status.code(), Some(7), "status: {out:?}");
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "boxed");

    // The program runs inside a box cgroup (a `.hexbox/<id>` path), not the host root.
    let out = box_run(
        &["--memory-max", "134217728"],
        &[
            "/bin/sh",
            "-c",
            "grep -o '[.]hexbox/[^ ]*' /proc/self/cgroup",
        ],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(".hexbox/"),
        "cgroup: {:?}",
        String::from_utf8_lossy(&out.stdout)
    );

    // A program over the box's memory bound is killed by the kernel (128 + SIGKILL = 137).
    let out = box_run(
        &["--memory-max", "67108864"],
        &[HEX, "selftest-box", "hog", "536870912"],
    );
    assert_eq!(
        out.status.code(),
        Some(137),
        "a 512 MiB hog in a 64 MiB box should be killed: {out:?}"
    );

    // An unknown flag is a typed refusal with the box-setup status (125), never a silent run.
    let out = box_run(&["--frobnicate", "1"], &["/bin/true"]);
    assert_eq!(out.status.code(), Some(125), "unknown flag: {out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("unknown flag"));
}

/// HEX-INST-6's other side (native plan step 6; the lead, 2026-11-06): `hex box run` wraps a program
/// for its caller, so the program keeps the caller's descriptor limit, not the default a box the node
/// starts gets (1024:524288). Under a caller's limit of one less than the default's soft limit, soft, and
/// the default's soft limit, hard (1023:1024: below the default in both, a lowering any caller may make),
/// its program sees exactly that.
#[test]
fn box_run_keeps_its_callers_descriptor_limit() {
    if let Err(why) = scope() {
        hex_testkit::skip(
            "box_run_keeps_its_callers_descriptor_limit",
            "a delegated cgroup or systemd --user",
            &format!("no delegated cgroup to run hex box run in ({why}); run on a cgroup-v2 host"),
        )
        .expect("recording the skip");
        return;
    }
    let hard = hex_core::limits::NOFILE_SOFT;
    let soft = hard - 1;
    let nofile = format!("--nofile={soft}:{hard}");
    let out = run_in_scope(
        "prlimit",
        &[
            nofile.as_str(),
            "--",
            HEX,
            "box",
            "run",
            "--",
            "/bin/sh",
            "-c",
            "echo $(ulimit -Sn):$(ulimit -Hn)",
        ],
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("{soft}:{hard}")
    );
}

/// AC-T7.1.1: run alone in a cgroup delegated to it (HEX_DELEGATED: a runner that holds a whole test
/// suite in one capped scope gives each test binary a cgroup of its own), every `hex box run` here and
/// its program stay inside that cgroup, under its caps, never in a scope of their own beside it.
#[test]
fn box_run_stays_inside_its_delegated_callers_cgroup() {
    let own = started_in();
    if std::env::var_os("HEX_DELEGATED").is_none() {
        hex_testkit::skip(
            "box_run_stays_inside_its_delegated_callers_cgroup",
            "HEX_DELEGATED",
            "not run alone in a delegated cgroup; a suite runner that gives each test binary one sets it",
        )
        .expect("recording the skip");
        return;
    }
    let out = box_run(
        &["--memory-max", "134217728"],
        &["/bin/cat", "/proc/self/cgroup"],
    );
    assert!(out.status.success(), "{out:?}");
    let program = cgroup_of(&String::from_utf8_lossy(&out.stdout));
    assert!(
        program.starts_with(&format!("{own}/")) && program.contains("/.hexbox/"),
        "the boxed program's cgroup {program} lies outside this binary's delegated {own}"
    );
}
