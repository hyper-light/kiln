//! Hex: the scheduler and runtime: agent scheduling orchestration for infinite scale. The `hex`
//! binary: it says what it is, and runs `selftest-box` acts inside a box (CP-T7.1). The runtime
//! classes are in `hex-runtime`, driven by the node agent in `hex-node`, which `hex node` runs as a
//! process serving its local socket.
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

pub mod selftest;
pub mod setup;

#[cfg(target_os = "macos")]
pub mod box_helper;
#[cfg(target_os = "linux")]
pub mod box_run;
#[cfg(target_os = "linux")]
pub mod guest;
#[cfg(target_os = "macos")]
pub mod managed_helper;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub mod node;
#[cfg(target_os = "linux")]
pub mod oci;
#[cfg(any(target_os = "linux", target_os = "macos", windows))]
pub mod schedule;

/// What `hex` prints for its arguments, where, and the exit status.
#[derive(Debug, PartialEq, Eq)]
pub struct Reply {
    pub text: String,
    pub to_stderr: bool,
    pub status: u8,
}

pub const USAGE: &str = "usage: hex [--version | --help]\n";

const HELP: &str =
    "hex: Hex, the scheduler and runtime: agent scheduling orchestration for infinite scale.\n";

/// The name, the version and the commit it was built from: `HEX_COMMIT` at build time, which
/// kiln CI sets, else "unknown commit".
#[must_use]
pub fn version_line() -> String {
    let commit = option_env!("HEX_COMMIT").unwrap_or("unknown commit");
    format!("hex {} ({commit})\n", env!("CARGO_PKG_VERSION"))
}

/// `hex keep`: the Windows node's keeper (`hex_runtime::windows::keeper`), a passive process holding its
/// boxes' job handles so they outlive the node. Its state folder and ready path come as flags or from the
/// environment the node starts it with; it runs until its stop is asked. 0 when it stopped cleanly, 2 for a
/// bad argument, 1 when it couldn't run.
#[cfg(windows)]
#[must_use]
pub fn keep(args: &[String]) -> u8 {
    use std::io::Write;
    let parsed = match hex_runtime::windows::keeper::Args::parse(args) {
        Ok(a) => a,
        Err(why) => {
            let _ = writeln!(std::io::stderr(), "hex keep: {why}");
            return 2;
        }
    };
    match hex_runtime::windows::keeper::run(&parsed) {
        Ok(()) => 0,
        Err(why) => {
            let _ = writeln!(std::io::stderr(), "hex keep: {why}");
            1
        }
    }
}

/// The reply to the binary's arguments; `None` stands for an argument that isn't Unicode.
#[must_use]
pub fn reply(args: &[Option<String>]) -> Reply {
    let refuse = |why: String| Reply {
        text: format!("hex: {why}\n{USAGE}"),
        to_stderr: true,
        status: 2,
    };
    match args {
        [Some(a)] if a == "--version" || a == "-V" => Reply {
            text: version_line(),
            to_stderr: false,
            status: 0,
        },
        [Some(a)] if a == "--help" || a == "-h" => Reply {
            text: format!("{HELP}{USAGE}"),
            to_stderr: false,
            status: 0,
        },
        [] => Reply {
            text: USAGE.to_owned(),
            to_stderr: true,
            status: 2,
        },
        _ if args.iter().any(Option::is_none) => refuse("an argument isn't Unicode".to_owned()),
        _ => refuse(format!(
            "unknown arguments: {}",
            args.iter().flatten().cloned().collect::<Vec<_>>().join(" ")
        )),
    }
}
