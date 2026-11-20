//! A Shards image VM's shares another server serves (Shards' D286; kiln goal 01a1ec34599974448f6b7663ddb07001;
//! docs/design/shards-image-class.md §2b): the node's server of them, its orchestrator's, bound once, and each one's
//! stream asked of it as the box boots, lowered to what Shards' image boot takes (`ImageSpec::served`).

use std::sync::Arc;

use hex_api::serve::Servers;
use hex_api::{BoxId, Refusal, ShardsImage};
use shards_api::image::{PlugFor, ServedShare, ServedStream};

use super::ImageClass;

impl ImageClass {
    /// The server its VMs' shares another server serves are asked of, `servers`, its orchestrator's, bound once: from
    /// then on a box naming one is created, and each one's stream asked of it as the box boots. Bound again, the same
    /// server is taken, as the warm room is ([`ImageClass::bind_room`]), and another refused `Broken`, the first kept.
    pub fn bind_servers(&self, servers: Arc<dyn Servers>) -> Result<(), Refusal> {
        let bound = self.servers.get_or_init(|| Arc::clone(&servers));
        if Arc::ptr_eq(bound, &servers) {
            Ok(())
        } else {
            Err(Refusal::Broken(
                "the image class's server of served shares cannot be rebound".to_owned(),
            ))
        }
    }

    /// Whether this node can serve `image`'s shares another server serves: none asks nothing; any asks for its server.
    pub(super) fn can_serve(&self, image: &ShardsImage) -> Result<(), Refusal> {
        if image.served.is_empty() || self.servers.get().is_some() {
            return Ok(());
        }
        Err(hex_api::serve::unconfigured())
    }

    /// Box `id`'s run's shares another server serves as Shards' image boot takes them: each one's stream asked of the
    /// node's server now, for this boot alone, in its spec's order; at its place, read-only as asked, its files run from
    /// it as every share's of the class are (D279), and given to the members it names as they start (D277). The streams
    /// go with the boot's spec, whose run host is handed its own copies with the run's request; this process's go as
    /// the boot answers, the share's resync channel with it where its server gives one (Shards' D288). The first a server
    /// refuses refuses the start, before any effect.
    pub(super) fn served(
        &self,
        id: &BoxId,
        image: &ShardsImage,
    ) -> Result<Vec<ServedShare>, Refusal> {
        if image.served.is_empty() {
            return Ok(Vec::new());
        }
        let servers = self
            .servers
            .get()
            .ok_or_else(hex_api::serve::unconfigured)?;
        let mut lowered = Vec::new();
        lowered
            .try_reserve_exact(image.served.len())
            .map_err(|_| Refusal::Exhausted {
                what: "served shares",
                bound: image.served.len(),
            })?;
        for share in &image.served {
            let (stream, resync) = servers.stream_resyncing(id, share)?;
            lowered.push(ServedShare {
                stream: ServedStream::new(stream),
                resync: resync.map(ServedStream::new),
                at: share.destination.clone(),
                read_only: share.read_only,
                exec: true,
                members: (!share.members.is_empty()).then(|| {
                    PlugFor::Members(
                        share
                            .members
                            .iter()
                            .map(super::image::member_name)
                            .collect(),
                    )
                }),
            });
        }
        Ok(lowered)
    }
}
