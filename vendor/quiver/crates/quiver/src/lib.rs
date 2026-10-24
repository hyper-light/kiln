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
//! Quiver: the topic store, the cornerstone for an event-driven world (docs/design/README.md).
//!
//! A [`Node`] holds topics of messages in this process and serves them through `quiver-api`'s
//! [`quiver_api::Topics`]: messages in order, read from any offset kept or followed by
//! subscriptions told what they missed, each message a Focal batch charged to Focal's memory budget
//! (vendor/focal-bus). It serves the ephemeral class today (kiln's CP-T2.1); STATUS.md says what
//! comes next. The `quiver` binary says what it is.

pub mod ephemeral;
pub mod node;

pub use node::{Clock, Node, NodeConfig, Stats, wall_clock};

/// What `quiver` prints for its arguments, where, and the exit status.
#[derive(Debug, PartialEq, Eq)]
pub struct Reply {
    pub text: String,
    pub to_stderr: bool,
    pub status: u8,
}

pub const USAGE: &str = "usage: quiver [--version | --help]\n";

const HELP: &str = "quiver: Quiver, the topic store: the cornerstone for an event-driven world.\n";

/// The name, the version and the commit it was built from: `QUIVER_COMMIT` at build time, which
/// kiln CI sets, else "unknown commit".
#[must_use]
pub fn version_line() -> String {
    let commit = option_env!("QUIVER_COMMIT").unwrap_or("unknown commit");
    format!("quiver {} ({commit})\n", env!("CARGO_PKG_VERSION"))
}

/// The reply to the binary's arguments; `None` stands for an argument that isn't Unicode.
#[must_use]
pub fn reply(args: &[Option<String>]) -> Reply {
    let refuse = |why: String| Reply {
        text: format!("quiver: {why}\n{USAGE}"),
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
