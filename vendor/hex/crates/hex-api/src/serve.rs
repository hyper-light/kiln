//! A Shards image VM's shares another server serves (Shards' D286; kiln goal 01a1ec34599974448f6b7663ddb07001): the seam
//! through which its node's server of such shares makes each one's stream as its box starts (CLAUDE.md §9). A box's spec
//! names each by its source, in that server's own words ([`crate::ServedMount`]); the stream is no part of any spec,
//! record or wire, since it is a descriptor of the node's own process, made for one boot alone. kiln's server serves an
//! agent's slates volume on the stream slates' daemon hands it (slates' Shards route, A-161), so its guest reads and
//! writes the volume, and maps its files through its share slot's DAX window, with no mount of the host's between them.
//!
//! - [`Servers`]: what a node's orchestrator gives it.
//! - [`sim::SimServers`], its simulator, and [`conformance`], the contract both it and every orchestrator's server pass.
//!
//! The node hands the stream to Shards' run host with the run's request (`shards_api::image::ImageSpec::served`) and
//! lets its own copy go once the boot has answered; the server sees the share end once every copy has closed, the
//! VM's with its run.

use crate::{BoxId, Refusal, ServedMount};

pub mod conformance;
pub mod sim;

/// A served share's stream: on Unix a connected stream socket's end, on Windows a duplex pipe's, as Shards takes one
/// (`shards_api::image::ServedStream`, its `shards run vm --slates-fs-fd` and `--slates-fs-handle`).
#[cfg(unix)]
pub type Stream = std::os::fd::OwnedFd;
/// A served share's stream: on Unix a connected stream socket's end, on Windows a duplex pipe's, as Shards takes one
/// (`shards_api::image::ServedStream`, its `shards run vm --slates-fs-fd` and `--slates-fs-handle`).
#[cfg(windows)]
pub type Stream = std::os::windows::io::OwnedHandle;

/// What a node's orchestrator gives it for its Shards image VMs' shares another server serves: each one's stream, made
/// as its box boots.
pub trait Servers: Send + Sync {
    /// The stream `share` of box `id` is served on, made now for this boot alone and given no other ask, served
    /// read-only where `share` says. Called on the boot's own thread, once for each of its served shares, in its spec's
    /// order, before any of its run starts. Refused, typed: `Invalid` where this server serves no `share.source`,
    /// naming it; `Exhausted` where it holds its most streams; `Os` where making one failed, in its words.
    fn stream(&self, id: &BoxId, share: &ServedMount) -> Result<Stream, Refusal>;
}

/// The refusal of a box naming a share another server serves, on a node given no server of them ([`Servers`]): its
/// configuration's, as the same node given one serves it. The image class and the simulator refuse it alike, as the box
/// is created, before anything is made.
pub fn unconfigured() -> Refusal {
    Refusal::Unconfigured {
        what: "server for its image VMs' served shares",
        why: "a Shards image VM given a share another server serves is handed that share's stream as it boots (Shards' \
              D286), and this node was given no server to ask one of"
            .to_owned(),
    }
}

#[cfg(test)]
mod tests;
