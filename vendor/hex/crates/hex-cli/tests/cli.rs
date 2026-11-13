//! The `hex` binary says what it is (kiln's CP-M1: the bar's first gated code), run as a process.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hex"))
        .args(args)
        .output()
        .expect("the binary runs")
}

#[test]
fn version_prints_the_name_version_and_commit() {
    let out = run(&["--version"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.starts_with(concat!("hex ", env!("CARGO_PKG_VERSION"), " (")),
        "{text}"
    );
    assert!(text.ends_with(")\n"), "{text}");
}

#[test]
fn help_prints_what_it_is_and_the_usage() {
    let out = run(&["--help"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.starts_with("hex: Hex"), "{text}");
    assert!(text.contains("usage: hex"), "{text}");
}

#[test]
fn an_unknown_argument_is_refused_with_the_usage() {
    let out = run(&["--frobnicate"]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let text = String::from_utf8(out.stderr).unwrap();
    assert!(text.contains("unknown arguments: --frobnicate"), "{text}");
    assert!(text.contains("usage: hex"), "{text}");
}

#[test]
fn no_argument_prints_the_usage_and_fails() {
    let out = run(&[]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(
        String::from_utf8(out.stderr)
            .unwrap()
            .starts_with("usage: hex")
    );
}

#[test]
fn an_argument_that_isnt_unicode_is_refused_not_a_panic() {
    assert_eq!(hex_cli::reply(&[None]).status, 2);
    assert!(hex_cli::reply(&[None]).text.contains("isn't Unicode"));
}

/// Goal 6 step 6's HEX-INST-5 (Ada's 2026-11-06 constants rule): the descriptors a node needs are said by the node, at
/// its own bounds on this host and for this process's owner, so an installer gives its service that and the install
/// lab checks it, never a figure copied from one host's (the lab's 594,304 was fedora's before the constants changed).
/// Do: run `hex node --descriptors`. Expect: `own N files M need T`, T the sum, N past the standard streams, and M some
/// for a root node, which keeps namespaces with their `/proc/net` files, none for a rootless one, which keeps none.
#[cfg(target_os = "linux")]
#[test]
fn a_node_says_the_descriptors_it_needs() {
    let out = run(&["node", "--descriptors"]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    let text = String::from_utf8(out.stdout).unwrap();
    let words: Vec<&str> = text.split_whitespace().collect();
    let ["own", own, "files", files, "need", need] = words.as_slice() else {
        panic!("{text:?}");
    };
    let (own, files, need): (u64, u64, u64) = (
        own.parse().unwrap(),
        files.parse().unwrap(),
        need.parse().unwrap(),
    );
    assert_eq!(need, own + files, "{text:?}");
    assert!(own > 3, "{text:?}");
    assert_eq!(
        files > 0,
        rustix::process::geteuid().is_root(),
        "a root node keeps namespaces with their files, a rootless one none: {text:?}"
    );
}

/// The native bench's acts (native plan step 7): do `climb` 3 MiB, expect it to report the KiB it has
/// touched after each 1 MiB step (its default, the instrument's resolution), one line each, and exit 0, so
/// a bound's overshoot is read from the program itself.
#[test]
fn climb_reports_each_step_it_touches() {
    let out = run(&["selftest-box", "climb", &(3u64 << 20).to_string()]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(String::from_utf8(out.stdout).unwrap(), "1024\n2048\n3072\n");
}

/// Do `climb` 2 MiB at half the default step, expect a line every 512 KiB: the bench reads a bound's
/// verdict at half its resolution with it.
#[test]
fn climb_takes_a_finer_step() {
    let out = run(&[
        "selftest-box",
        "climb",
        &(2u64 << 20).to_string(),
        &(512u64 << 10).to_string(),
    ]);
    assert_eq!(out.status.code(), Some(0), "{out:?}");
    assert_eq!(
        String::from_utf8(out.stdout).unwrap(),
        "512\n1024\n1536\n2048\n"
    );
}

/// Do `serve`, expect "up" on stdout once it runs, and it to keep running until it is killed: a start
/// is timed to that line, the same for every driver.
#[test]
fn serve_says_up_and_runs_on() {
    use std::io::{BufRead, BufReader};
    let mut child = Command::new(env!("CARGO_BIN_EXE_hex"))
        .args(["selftest-box", "serve"])
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    BufReader::new(child.stdout.take().unwrap())
        .read_line(&mut line)
        .unwrap();
    assert_eq!(line, "up\n");
    assert!(child.try_wait().unwrap().is_none(), "serve ended");
    child.kill().unwrap();
    child.wait().unwrap();
}

/// HEX-INST-9 (native plan step 6): `hex setup features` runs the node's own bounded feature query and
/// prints each feature's state, one `<name> on|off` line each, Containers then HypervisorPlatform, for the
/// Windows installer to read; off Windows it is refused by name, naming the platform.
#[test]
fn setup_features_prints_each_features_state_or_refuses_off_windows() {
    let out = run(&["setup", "features"]);
    if cfg!(windows) {
        assert_eq!(out.status.code(), Some(0), "{out:?}");
        let text = String::from_utf8(out.stdout).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text:?}");
        for (line, name) in lines.iter().zip(["Containers", "HypervisorPlatform"]) {
            let state = line.strip_prefix(name).and_then(|r| r.strip_prefix(' '));
            assert!(matches!(state, Some("on" | "off")), "{line:?}");
        }
    } else {
        assert_eq!(out.status.code(), Some(2), "{out:?}");
        let err = String::from_utf8(out.stderr).unwrap();
        assert!(
            err.contains(&format!("isn't available on {}", std::env::consts::OS)),
            "{err}"
        );
    }
}
