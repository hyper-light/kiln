//! kiln's server of its microVMs' folders another server serves (Hex's `hex_api::serve::Servers`, bound to the image
//! runtime in [`super::held_in`]; Shards' D286; kiln goal 01a1ec34599974448f6b7663ddb07001): each source is a Slates
//! volume of this machine's instance, named by its id (`crate::workspace::slates::Made::volume`), and its stream is the
//! one Slates' daemon hands this process for it (the Shards route, Slates' A-161: `Instance::serve_shards`). Hex hands
//! that stream to Shards' run host and the run host to its microVM, whose virtio-fs device and DAX window Slates
//! answers on it, so an isolated agent works in its volume with nothing of it mounted on this machine.
//!
//! Slates serves the attachment until every copy of the stream has closed: Hex lets this process's go as the boot
//! answers, so the attachment ends with the VM's run. Each ask is one client of its own, on Hex's boot thread, never
//! kiln's runtime.

use hex_api::Refusal;

/// The server of folders Slates serves a microVM: each a volume of this machine's instance (module docs).
#[derive(Debug, Clone, Copy, Default)]
pub struct SlatesVolumes;

impl hex_api::serve::Servers for SlatesVolumes {
    fn stream(&self, _id: &hex_api::BoxId, share: &hex_api::ServedMount) -> Result<hex_api::serve::Stream, Refusal> {
        let unserved = |why: String| Refusal::Invalid { what: "a served share", why };
        let volume = crate::workspace::slates::volume_id(&share.source).map_err(|e| unserved(format!("Slates serves no volume {:?}: {e:#}", share.source)))?;
        let intent = if share.read_only { slates_api::Intent::Read } else { slates_api::Intent::Write };
        let mut slates = slates_api::Instance::attach(&crate::workspace::slates::instance()).map_err(|e| Refusal::Os { op: "reaching this machine's Slates", why: e.to_string() })?;
        match slates.serve_shards(volume, intent, None) {
            Ok(served) => Ok(served.stream),
            Err(slates_api::Error::NotFound(slates_api::Missing::Volume(_))) => Err(unserved(format!("Slates serves no volume {}", share.source))),
            Err(e) => Err(Refusal::Os { op: "asking Slates for a served share's stream", why: e.to_string() }),
        }
    }
}
