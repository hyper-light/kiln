//! C-1 (decision 9; the person's rule of 2026-11-13; AC-T7.1.3): a run-out is told once where a person sees it, the
//! process's log (its stderr: a node's journal, a service's log, a test's output), in every configuration: with no
//! census installed, as a node runs in production. Its own process, as the told line is the process's own stderr.
#![allow(clippy::disallowed_macros)]

use std::process::Command;
use std::time::Duration;

use hex_api::patience::{Patience, Piece};

/// Set in the child this test starts of itself: there it runs the wait out.
const CHILD: &str = "HEX_TEST_PATIENCE_TOLD_CHILD";

/// Do: in a process of its own with no census, answer a wait named `told.test` (W* = that answer), then run it out
/// past W*. Expect: the process's stderr carries one line telling it, naming the wait, its W*, how long it waited and
/// the party it follows from then on, and no other line for it.
#[test]
fn a_run_out_is_told_once_on_the_processs_log_with_no_census() {
    let answer = Duration::from_millis(3);
    let waited = Duration::from_millis(5);
    if std::env::var_os(CHILD).is_some() {
        let p = Patience::measuring("told.test");
        p.answered(answer);
        p.ran_out(answer, waited, &[Piece::Kind("the test's stand-in")]);
        return;
    }
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "a_run_out_is_told_once_on_the_processs_log_with_no_census",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "the child failed: {stderr}");
    let told: Vec<&str> = stderr
        .lines()
        .filter(|line| line.contains("wait=told.test"))
        .collect();
    assert_eq!(
        told,
        [format!(
            "hex.wait event=ran_out wait=told.test w_star_ns={} waited_ns={} party=\"the test's stand-in\"",
            answer.as_nanos(),
            waited.as_nanos()
        )],
        "the run-out told once on stderr, naming the wait, its W*, how long and its party; stderr: {stderr}"
    );
}
