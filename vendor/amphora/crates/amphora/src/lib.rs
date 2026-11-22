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
//! Amphora: the database kiln keeps every record in: KilnDB made a product (docs/design/README.md,
//! STATUS.md).
//!
//! - [`audit`]: the audit log (CP-T1.7): RFC 6962's Merkle tree over Records, verified on read.
//! - [`store`]: a database's commit pipeline and its readers (CP-T1.1): one writer with a group
//!   commit and no timer, durable beside it by a flusher, readers on connections of their own, and
//!   no checkpoint in a commit's way.
//! - [`groups`]: a store in groups on one node, keys spread over them by hash, so writes go on in
//!   parallel.
//! - [`reshard`]: groups that split, merge and move themselves by measured size and load, fenced,
//!   by copy, catch-up and a brief cutover (CP-T1.9).
//! - [`db`]: stores on Amphora's engine (`crates/engine`).
//! - [`meter`]: what a store counts.
//! - [`regions`]: region-local groups under a global root (CP-T1.11): region labels, placement
//!   rules, routing that tries the reader's region first, and the simulated region lab.
//! - [`xgroup`]: transactions across groups (CP-T1.10): two-phase commit and Parallel Commits.
//! - [`memory`]: the memory this process may use, as the system says (the process's page pool is a
//!   part of it).
//!
//! The `amphora` binary says what it is ([`reply`]).

pub mod audit;
pub mod backup;
pub mod database;
pub mod db;
mod descriptors;
pub mod groups;
pub mod memory;
pub mod meter;
pub mod regions;
pub mod replication;
pub mod reshard;
pub mod search;
pub mod sql;
pub mod store;
pub mod transport;
pub mod xgroup;

/// What `amphora` prints for its arguments, where, and the exit status.
#[derive(Debug, PartialEq, Eq)]
pub struct Reply {
    pub text: String,
    pub to_stderr: bool,
    pub status: u8,
}

pub const USAGE: &str = "usage: amphora [--version | --help]\n";

const HELP: &str =
    "amphora: Amphora, the database kiln keeps every record in: KilnDB made a product.\n";

/// The name, the version and the commit it was built from: `AMPHORA_COMMIT` at build time, which
/// kiln CI sets, else "unknown commit".
#[must_use]
pub fn version_line() -> String {
    let commit = option_env!("AMPHORA_COMMIT").unwrap_or("unknown commit");
    format!("amphora {} ({commit})\n", env!("CARGO_PKG_VERSION"))
}

/// The reply to the binary's arguments; `None` stands for an argument that isn't Unicode.
#[must_use]
pub fn reply(args: &[Option<String>]) -> Reply {
    let refuse = |why: String| Reply {
        text: format!("amphora: {why}\n{USAGE}"),
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
