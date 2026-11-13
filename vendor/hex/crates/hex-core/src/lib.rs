//! Hex's sans-IO cores (CLAUDE.md §9): what to write to a box's cgroup for its bounds, what its
//! cgroup files say it used, the runtime-spec bundle an `oci` box's structured spec renders to, the
//! record the node agent keeps of each box, the plan that takes every box back or removes it after
//! the node agent crashed (T-T7.1.2), the pool of network namespaces it makes ahead for its
//! containers ([`netns`]), the seeds it keeps to clone them ([`seeds`]), the idle cgroups and network
//! namespaces it hands from box to box of one owner ([`reuse`], [`netns_reuse`]), the sizes its pools learn from
//! demand ([`demand`]), how long each of its waits for an answer lasts, learned from the answers before it
//! ([`patience`]), the node agent's socket as bytes ([`wire`]), the rule a Windows `oci` box is made by
//! ([`hcs`]), the rules a macOS box is bounded by — rootless by polling ([`watch`]) and under a privileged helper
//! by the kernel ([`boxuser`]) — how its memory bound is apportioned across its members for the kernel to enforce
//! ([`ledger`]), macOS's signal numbers ([`darwin`]), how long a killed group gets to empty ([`drain`]), the macOS
//! box helper's protocol ([`helper`]) and the box users' pool it grows ([`pool`]), the
//! descriptor limit a box gets where its spec names none ([`limits`]) and what each OS's installer sets
//! up and the refusal where it is missing ([`setup`]). Shells in `hex-runtime`, `hex-node` and
//! `hex-client` do the I/O these decide.

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

pub mod boxuser;
pub mod bundle;
pub mod capacity;
pub mod cdi;
pub mod cgroup;
pub mod darwin;
pub mod demand;
pub mod devfs;
pub mod drain;
pub mod hcs;
pub mod helper;
pub mod job;
pub mod lanes;
pub mod ledger;
pub mod limits;
#[cfg(test)]
mod meet;
pub mod netns;
pub mod netns_reuse;
pub use hex_api::patience;
pub mod pool;
pub mod record;
pub mod recovery;
pub mod reuse;
pub mod schedule;
pub mod seed;
pub mod seeds;
pub mod setup;
pub mod stream;
pub mod watch;
pub mod wire;
