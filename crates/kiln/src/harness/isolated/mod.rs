//! What kiln assumes of the machine a harness runs on, held for a harness in a microVM of its own
//! (kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 18). A process on its machine shares that
//! machine's files, programs and sockets with kiln; in a microVM it shares only what it's given:
//!
//! - [`home`]: its harness's configuration (its sign-in, settings, transcripts) on a volume kiln
//!   keeps on its machine for the person it acts for, one per person and harness, which kiln reads
//!   from the machine for its transcripts and resume as it reads a harness's home today.
//! - [`env`]: kiln's own variables (`KILN_*`) as a process on its machine gets them, less those
//!   that name what isn't in the microVM, and nothing of the machine's own environment.
//! - kiln's MCP server isn't here, and no Agentfile grants anything for it: it rides the link kiln
//!   drives the harness by, as that link's own `mcp` lane (Shards D254), never the network (ROOT's
//!   decision, 2026-11-13). Its in-microVM endpoint and the runner's in-process bridge to kiln's MCP
//!   are owed with the lane.
//! - [`host_tools`]: a session whose project builds with its host's own tools (Xcode, MSVC) can't
//!   run in a Linux microVM: refused, saying so, where isolation is asked for.
//!
//! Each is a pure function of what it's given, or a refusal ([`Refusal`]) the caller matches (R04).
//! The launch through Hex (step 17) calls them as it starts the microVM; the identities and the
//! terminal lane that sign-in, git signing and credential helpers need are steps 6 and 11's
//! (docs/agents.md, "In a microVM of its own").

pub mod env;
pub mod home;
pub mod host_tools;

/// Why an isolated session can't have what a process on its machine has. Each says why in words
/// the person reads; callers match the variant.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    /// Its person isn't one kiln keeps a harness's configuration for.
    #[error("{0:?} names no person kiln keeps a harness's configuration for (`owner`, or `user:<id>` with an id of letters, digits, `.`, `_` and `-`)")]
    Person(String),
    /// Its harness isn't one kiln runs.
    #[error("kiln runs no harness {0:?}")]
    Harness(String),
    /// A volume's path in its image isn't one every machine can keep a directory for.
    #[error("its image's volume {path:?} can't be kept on this machine: {why}")]
    Volume { path: String, why: &'static str },
    /// None of its image's volumes holds what its harness's environment names (`HOME`,
    /// `CODEX_HOME` ...): its harness would keep its configuration, sign-in and transcripts in the
    /// microVM's scratch, gone when it stops, and nothing kiln reads of a harness could be read.
    #[error("{harness}'s image keeps its configuration in no volume (its environment names {named}): its sign-in and transcripts would go with each microVM, and kiln could read none of them")]
    NoConfigVolume { harness: String, named: String },
    /// Its project builds with tools only its host's own system has.
    #[error("{}", host_tools::refusal_words(.0))]
    HostTools(Vec<host_tools::HostTool>),
}

#[cfg(test)]
mod tests;
