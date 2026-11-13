//! The delegated cgroup's tests (ROOT's finding of 2026-11-13): a test binary's tests run on threads of one process,
//! and a cgroup holds a process whole, so its preparation is the process's one. Each case runs in a process of its
//! own, in a fresh scope delegated to it ([`alone`]), so the case's preparation is its process's first.

use std::process::{Child, Command};
use std::sync::{Arc, Barrier};

use super::*;

/// Names the case a run of this binary is for, in a scope of its own: set only there.
const CASE: &str = "HEX_TESTKIT_CGROUP_CASE";

/// Runs `case` alone, in a process of its own in a fresh cgroup delegated to it: this binary run again as the test
/// `name` alone in a transient `systemd-run --user --scope -p Delegate=yes`, [`CASE`] naming it. Where no systemd
/// --user manager answers (its `$XDG_RUNTIME_DIR/systemd/private` socket isn't there), the skip is recorded.
fn alone(name: &str, case: impl FnOnce()) {
    if std::env::var_os(CASE).is_some_and(|c| c == name) {
        case();
        return;
    }
    let manager = std::env::var_os("XDG_RUNTIME_DIR")
        .map(|d| Path::new(&d).join("systemd/private"))
        .is_some_and(|p| p.exists());
    if !manager {
        crate::skip(
            name,
            "a systemd --user manager",
            "no fresh scope can be delegated to the case",
        )
        .unwrap();
        return;
    }
    let status = Command::new("systemd-run")
        .args(["--user", "--scope", "--quiet", "-p", "Delegate=yes"])
        .arg(std::env::current_exe().unwrap())
        .args([name, "--exact", "--nocapture", "--test-threads=1"])
        .env(CASE, name)
        .status()
        .unwrap();
    assert!(status.success(), "{name} failed in a scope of its own");
}

/// Whether `cgroup` is handed a domain controller (memory or io), which the kernel won't enable for the cgroup's
/// children while it holds a process; a case without one records its skip, as it can show nothing.
fn holds_a_domain_controller(name: &str, cgroup: &Path) -> bool {
    let listed = fs::read_to_string(cgroup.join("cgroup.controllers")).unwrap();
    let held = listed
        .split_whitespace()
        .any(|c| c == "memory" || c == "io");
    if !held {
        crate::skip(
            name,
            "a domain controller (memory or io) delegated to the scope",
            &format!("{} hands down only {listed:?}", cgroup.display()),
        )
        .unwrap();
    }
    held
}

/// The probe: every controller delegated to `own` enabled for its children, as a box root's opening does.
fn enable_controllers(own: &Path) -> Result<(), String> {
    let listed = fs::read_to_string(own.join("cgroup.controllers"))
        .map_err(|e| format!("reading {}'s controllers: {e}", own.display()))?;
    let enable: Vec<String> = listed.split_whitespace().map(|c| format!("+{c}")).collect();
    fs::write(own.join("cgroup.subtree_control"), enable.join(" "))
        .map_err(|e| format!("enabling {} in {}: {e}", enable.join(" "), own.display()))
}

/// The cgroup directory process `pid` is in, as its /proc/<pid>/cgroup names it.
fn cgroup_of(pid: u32) -> PathBuf {
    let listed = fs::read_to_string(format!("/proc/{pid}/cgroup")).unwrap();
    let rel = listed.lines().find_map(|l| l.strip_prefix("0::")).unwrap();
    Path::new("/sys/fs/cgroup").join(rel.trim_start_matches('/'))
}

/// A child, killed and reaped when dropped, so a failed case leaves no process holding its scope.
struct Reaped(Child);

impl Drop for Reaped {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// ROOT's finding of 2026-11-13 (oci_box's main test skipped, its scope busy): a test's child started before another
/// test's preparation (oci_box's `id -u`) stayed in the process's cgroup when the process moved into its leaf, and the
/// kernel refuses to enable a controller for a cgroup's children while it holds a process (EBUSY), so the
/// preparation failed. Do: in a scope of its own, start a child, then prepare. Expect: the preparation done, its
/// cgroup the one the process started in, and the child in the leaf with the process.
#[test]
fn a_child_started_before_the_preparation_moves_into_the_leaf_with_the_process() {
    const NAME: &str = "cgroup::tests::a_child_started_before_the_preparation_moves_into_the_leaf_with_the_process";
    alone(NAME, || {
        let started_in = own().unwrap();
        if !holds_a_domain_controller(NAME, &started_in) {
            return;
        }
        let child = Reaped(Command::new("sleep").arg("infinity").spawn().unwrap());
        let prepared = delegated(enable_controllers);
        assert_eq!(prepared, Ok(started_in.clone()));
        assert_eq!(cgroup_of(child.0.id()), started_in.join(LEAF));
    });
}

/// ROOT's finding of 2026-11-13: a test binary's tests prepare from threads of one process, and a preparation moves the
/// whole process into the leaf, so one that read the process's cgroup after another's move took the leaf for its own.
/// Do: in a scope of its own, prepare from two threads at once, then once more. Expect: each the cgroup the process
/// started in.
#[test]
fn two_threads_preparing_at_once_and_a_later_call_get_the_one_cgroup() {
    const NAME: &str =
        "cgroup::tests::two_threads_preparing_at_once_and_a_later_call_get_the_one_cgroup";
    alone(NAME, || {
        let started_in = own().unwrap();
        if !holds_a_domain_controller(NAME, &started_in) {
            return;
        }
        let at_once = Arc::new(Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|_| {
                let at_once = Arc::clone(&at_once);
                std::thread::spawn(move || {
                    at_once.wait();
                    delegated(enable_controllers)
                })
            })
            .collect();
        let mut prepared: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        prepared.push(delegated(enable_controllers));
        let one = Ok(started_in);
        assert_eq!(prepared, [one.clone(), one.clone(), one]);
    });
}
