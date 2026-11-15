//! C-1 (decision 9; AC-T7.1.3): the waits census is an opt-in record that disarms no wait. A node with
//! `HEX_WAITS_CENSUS` set keeps every learned wait armed at its W*, so its run-outs are told and recorded as anywhere
//! else, and the record still has each answer's tail: a wait past W* follows its party to its answer, uncensored.
//! Its own process, as the census is installed for the process's whole life.
#![allow(clippy::disallowed_macros)]

use std::process::Command;
use std::time::Duration;

use hex_core::patience::{Patience, Piece};

/// Set in the child this test starts of itself: there it installs the census and runs a wait.
const CHILD: &str = "HEX_TEST_CENSUS_CHILD";

/// Do: in a process of its own with the census installed from its variables, answer a wait named `census.armed`, ask
/// its deadline, then run it out past that. Expect: the wait armed at its answer, the slowest seen; the census file
/// holding the answer and the run-out, in the census's form.
#[test]
fn the_census_records_every_wait_and_disarms_none() {
    let answer = Duration::from_millis(3);
    let waited = Duration::from_millis(5);
    if std::env::var_os(CHILD).is_some() {
        assert_eq!(hex_node::census::install_from_env(), Ok(true));
        let p = Patience::measuring("census.armed");
        p.answered(answer);
        assert_eq!(
            p.deadline(),
            Some(answer),
            "the census disarmed a learned wait"
        );
        p.ran_out(answer, waited, &[Piece::Kind("the test's stand-in")]);
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("census");
    let written = [
        hex_core::patience::census_line(
            "census.armed",
            hex_core::patience::Sample::Answered(answer),
        ),
        hex_core::patience::census_line("census.armed", hex_core::patience::Sample::RanOut(answer)),
    ];
    let out = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "the_census_records_every_wait_and_disarms_none",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(CHILD, "1")
        .env(hex_node::census::ENV_FILE, &file)
        .env(hex_node::census::ENV_MAX, written.len().to_string())
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "the child failed: {stderr}");
    assert_eq!(std::fs::read_to_string(&file).unwrap(), written.concat());
}
