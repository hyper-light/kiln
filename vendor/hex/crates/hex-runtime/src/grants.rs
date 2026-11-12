//! A box's network grants as Shards' image boot (D231) takes them (joint plan step 15; docs/design/network-grants.md
//! N4): exactly what Shards `482c5d78` can hold, or a typed refusal naming the capability Hex awaits. Pure: the Shards
//! image class hands [`ImageNetwork`] to `shards_api::image::ImageSpec` as it boots, and records what the boot bound.

use hex_api::{
    Destination, Grant, MemberKind, Proto, Published, Receiver, Refusal, ShardsImage, Upstream,
    check_grants,
};
use shards_api::image::{Bound, Keep, MemberInfo, Narrowing, Publish};

/// What an image boot is told of a box's grants: the ports it publishes, and how the image's own grants past the
/// microVM are narrowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageNetwork {
    pub publish: Vec<Publish>,
    pub egress: Narrowing,
}

/// `image`'s grants as its boot takes them, or the refusal of the first one 482c5d78 can't hold exactly:
/// - a name, for any member: a harness's egress is ports to every address and its DNS any name (Shards'
///   harness-bringup-v1, H2), and an agent's remote MCP grant is kept image-wide, its member's scope out of sight;
/// - an address, and the VM's own host: no grant of either form exists;
/// - a publish, on a run with a command of its own: that command takes every port its Agentfile gives no member
///   (D122), and Shards names no port's receiver, so the grant's member can't be held (N6).
///
/// The image's own grants are never kept, where `Narrowing::default()` would keep them whole. So with no grant the
/// run has no network at all (`shards api-run`'s `network none`), and every published port is a grant's, at the host
/// address it names.
pub fn program(image: &ShardsImage) -> Result<ImageNetwork, Refusal> {
    let grants = image.grants.as_slice();
    check_grants(grants)?;
    let mut publish = Vec::new();
    publish
        .try_reserve_exact(grants.len())
        .map_err(|_| Refusal::Exhausted {
            what: "published ports",
            bound: grants.len(),
        })?;
    for grant in grants {
        let needs = match grant {
            Grant::Egress {
                to: Destination::Name(_),
                ..
            } => Upstream::MemberGrantsByName,
            Grant::Egress {
                to: Destination::Address(_),
                ..
            } => Upstream::MemberGrantsByAddress,
            Grant::Host { .. } => Upstream::MemberHostService,
            Grant::Publish { .. } if image.command.is_some() => Upstream::PublishReceiver,
            Grant::Publish {
                proto,
                port,
                host,
                host_port,
                ..
            } => {
                publish.push(Publish {
                    guest_port: *port,
                    proto: match proto {
                        Proto::Tcp => shards_api::image::Proto::Tcp,
                        Proto::Udp => shards_api::image::Proto::Udp,
                    },
                    host: Some(*host),
                    // shards-api's word for a port the host's OS picks.
                    host_port: host_port.unwrap_or(0),
                });
                continue;
            }
        };
        return Err(Refusal::Awaits { needs });
    }
    Ok(ImageNetwork {
        publish,
        egress: Narrowing {
            egress: Keep::Only(Vec::new()),
            mcp: Keep::Only(Vec::new()),
            destinations: Keep::Only(Vec::new()),
            dns: false,
        },
    })
}

/// A booted VM's publishes as its run reports them, held to its grants (N6): each publish grant's member is one its
/// image runs, and the host bound each publish grant's port once, where the grant names it, and nothing else. Returns
/// each as bound, its receiver the image's declaration, which Shards names no further yet. A grant naming a member
/// the image doesn't run is the spec's fault (`Invalid`); a socket bound past or short of the grants, Shards'
/// (`Broken`).
pub fn bound(
    grants: &[Grant],
    members: &[MemberInfo],
    published: &[Bound],
) -> Result<Vec<Published>, Refusal> {
    let room = |what: &'static str, bound: usize| Refusal::Exhausted { what, bound };
    let mut made = Vec::new();
    made.try_reserve_exact(published.len())
        .map_err(|_| room("published ports", published.len()))?;
    let mut used = Vec::new();
    used.try_reserve_exact(published.len())
        .map_err(|_| room("published ports", published.len()))?;
    used.resize(published.len(), false);
    for (at, grant) in grants.iter().enumerate() {
        let Grant::Publish {
            member,
            proto,
            port,
            host,
            host_port,
        } = grant
        else {
            continue;
        };
        let kind = match member.kind {
            MemberKind::Agent => shards_api::image::MemberKind::Agent,
            MemberKind::Harness => shards_api::image::MemberKind::Harness,
        };
        if !members
            .iter()
            .any(|m| m.kind == kind && m.name == member.name && m.runs)
        {
            return Err(Refusal::Invalid {
                what: "grants",
                why: format!("grants[{at}].member {member} isn't a member its image runs"),
            });
        }
        let proto = match proto {
            Proto::Tcp => shards_api::image::Proto::Tcp,
            Proto::Udp => shards_api::image::Proto::Udp,
        };
        let found = published.iter().zip(used.iter_mut()).find(|(b, used)| {
            !**used
                && b.guest_port == *port
                && b.proto == proto
                && b.host.ip() == *host
                && host_port.is_none_or(|wanted| wanted == b.host.port())
        });
        let Some((socket, taken)) = found else {
            return Err(Refusal::Broken(format!(
                "Shards bound no host socket for grants[{at}]"
            )));
        };
        *taken = true;
        made.push(Published {
            grant: u32::try_from(at).map_err(|_| room("grants", at))?,
            at: socket.host,
            receiver: Receiver::Declared,
        });
    }
    if let Some((extra, _)) = published.iter().zip(&used).find(|(_, used)| !**used) {
        return Err(Refusal::Broken(format!(
            "Shards published guest port {} ({:?}) at {}, which no grant names",
            extra.guest_port, extra.proto, extra.host
        )));
    }
    Ok(made)
}

#[cfg(test)]
mod tests;
