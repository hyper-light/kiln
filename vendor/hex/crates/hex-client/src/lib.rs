//! Hex's `Runtime` seam over the node agent's local socket (decision 3 of
//! docs/design/runtime-architecture.md): a [`Client`] is a `hex_api::Runtime` whose boxes are a node
//! agent's in another process, reached through its Unix `SOCK_SEQPACKET` socket in
//! [`hex_core::wire`]'s format, so a caller that doesn't link Hex holds the same contract as one that
//! does (the conformance suite, HEX-RT-1 on). An exec's stdio goes to the agent as descriptors, and
//! its `Proc` waits for the program's end, which the agent sends. A stream the caller asks for comes back as
//! its end with the answer; one whose program can't take the caller's own pipe (a guest's) comes back as its
//! end too, and the client copies between it and the pipe: the one copy left is the caller's (native plan
//! step 8). Every thread sharing a client shares
//! its one connection, which closes once the client and the last `Proc` it returned are dropped; the
//! agent then ends the programs it still watches for it (the disconnect rule).
//!
//! On Linux the socket is `SOCK_SEQPACKET` (unix(7)). macOS and Windows have only `SOCK_STREAM`, so each message
//! is framed there: macOS's descriptors ride at its first byte; Windows' given handles ride in its envelope
//! (native plan step 8). Every transport uses the same reader's turn (`calls.rs`): whoever waits reads, one at
//! a time, and no client reader thread sits between. Elsewhere [`Client::connect`] refuses, naming the platform.
//! On every platform, [`guest::Client`] reaches a node agent in a Shards guest over a byte stream
//! (docs/design/shards-guest.md §3), the same calls on another transport.

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

mod calls;
mod copy;
pub mod guest;

pub use calls::{Deferred, Ended, Then};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use calls::WIRE_EXEC_LIMIT;
#[cfg(target_os = "linux")]
pub use linux::Client;

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub use macos::Client;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::Client;

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
mod other;
#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
pub use other::Client;

/// Most boxes one list takes, in however many messages they come: a node holding more is past any
/// density Hex targets (10,560 idle containers per GiB held, docs/design/runtime-architecture.md),
/// so only a hostile agent sends them, and the list is refused.
pub const MAX_LIST: usize = 1 << 20;
