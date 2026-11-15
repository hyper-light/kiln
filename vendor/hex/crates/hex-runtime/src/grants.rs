//! A box's network grants as Shards' image boot (D231) takes them (joint plan step 15; docs/design/network-grants.md
//! N4 and N6): exactly what Shards at the revision vendor/shards pins can hold, its run host holding each member to its
//! reach of the names kept (D246), or a typed refusal naming the capability Hex awaits. Pure: the Shards image class hands [`ImageNetwork`] to
//! `shards_api::image::ImageSpec` as it boots, and holds the run to its grants as the boot returns ([`bound`]).

use std::borrow::Cow;

use hex_api::{
    Destination, Grant, MemberKind, Proto, Published, Receiver, Refusal, ShardsImage, check_grants,
    unheld,
};
use shards_api::image::{Bound, Keep, MemberInfo, MemberReach, Narrowing, Publish, Receiving};

/// What an image boot is told of a box's grants: the ports it publishes, and how the image's own grants past the
/// microVM are narrowed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageNetwork {
    pub publish: Vec<Publish>,
    pub egress: Narrowing,
}

/// `image`'s grants as its boot takes them, or the refusal of what D236's destinations can't express at all
/// ([`hex_api::unheld`], N4). Each destination a grant names by TCP (a host's name, an IPv4 address, the host itself)
/// is kept as D236 writes it, `host:port`, one for each port of the grant's range, and every member is held to its own
/// on every boot (`Narrowing::members`, D246): the run host refuses the boot, before anything starts, where any member
/// the image declares would reach other than that of the names kept, a member in no entry nothing. A publish is
/// programmed on any run: Shards names whom each reaches as the boot returns, and [`bound`] holds the run to them there
/// (N6).
///
/// Nothing else of the image's grants is kept, where `Narrowing::default()` would keep them whole: no port egress, no
/// remote MCP server (so of D246's names kept, destinations and remote MCP servers alike, every one is a grant's
/// destination), no name past the VM but the destinations'. So with no grant nothing passes the microVM: the run has
/// no network, or, where its image links its members past the microVM, one whose network process lets nothing past
/// (D250); and every published port is a grant's, at the host address it names.
pub fn program(image: &ShardsImage) -> Result<ImageNetwork, Refusal> {
    let grants = image.grants.as_slice();
    check_grants(grants)?;
    if let Some(needs) = unheld(grants) {
        return Err(Refusal::Awaits { needs });
    }
    // The destinations ride the boot's one request (D231), which carries at most `run::MAX_PAYLOAD` bytes: a list naming
    // more is refused by their bytes, counted before any port is expanded (the review's D-9; N7).
    let bytes = request_bytes(grants)?;
    let most = u64::from(shards_abi::run::MAX_PAYLOAD);
    if bytes > most {
        return Err(Refusal::Invalid {
            what: "grants",
            why: format!(
                "they name destinations taking {bytes} bytes of an image boot's request, kept and in each member's \
                 reach, past the {most} its channel carries (Shards' run::MAX_PAYLOAD): name fewer ports or hosts"
            ),
        });
    }
    let (kept, members) = reach(grants)?;
    let mut publish = Vec::new();
    publish
        .try_reserve_exact(grants.len())
        .map_err(|_| Refusal::Exhausted {
            what: "published ports",
            bound: grants.len(),
        })?;
    for grant in grants {
        if let Grant::Publish {
            proto,
            port,
            host,
            host_port,
            ..
        } = grant
        {
            publish.push(Publish {
                guest_port: *port,
                proto: shards_proto(*proto),
                host: Some(*host),
                // shards-api's word for a port the host's OS picks.
                host_port: host_port.unwrap_or(0),
            });
        }
    }
    Ok(ImageNetwork {
        publish,
        egress: Narrowing {
            egress: Keep::Only(Vec::new()),
            mcp: Keep::Only(Vec::new()),
            destinations: Keep::Only(kept),
            dns: false,
            members: Some(members),
        },
    })
}

/// A list's head as D231's channel writes it (shards-api's image wire): a tag byte and a u32 count.
const LIST_HEAD: u64 = 1 + 4;
/// A member's head in its reach's list: its kind's byte, its name's u32 length, and its destinations' u32 count.
const MEMBER_HEAD: u64 = 1 + 4 + 4;
/// A destination's bytes beside its host's and its port's digits: its text's u32 length and the colon.
const DESTINATION_HEAD: u64 = 4 + 1;

/// The bytes D231's channel writes for the destinations `grants` name and each member's reach of them (shards-api's
/// `Request::encode`: the kept list as a tag, a count and each destination as a u32 length and its text; the members as
/// a tag and a count, then each as its kind, its name, a count and its destinations alike), counted from the grants'
/// ranges, never a port expanded: a destination per port, `<host>:<port>`, kept once however many members reach it
/// (the union of the ranges naming its host), and once in each member's reach (no grant names a member's flow twice,
/// `check_grants`). Saturating: a count past a u64 is past any bound it is held to. Its two working lists, one entry a
/// grant, are reserved fallibly, refused `Exhausted` past what memory gives (N7).
fn request_bytes(grants: &[Grant]) -> Result<u64, Refusal> {
    let exhausted = |bound: usize| Refusal::Exhausted {
        what: "destinations",
        bound,
    };
    let mut by_member: Vec<(MemberKind, &str, u64)> = Vec::new();
    by_member
        .try_reserve_exact(grants.len())
        .map_err(|_| exhausted(grants.len()))?;
    let mut by_host: Vec<(Cow<'_, str>, u16, u16)> = Vec::new();
    by_host
        .try_reserve_exact(grants.len())
        .map_err(|_| exhausted(grants.len()))?;
    for grant in grants {
        let (member, host, ports) = match grant {
            Grant::Egress {
                member,
                to: Destination::Name(name),
                ports,
            } => (member, Cow::Borrowed(name.as_str()), ports),
            Grant::Egress {
                member,
                to: Destination::Address(address),
                ports,
            } => (member, Cow::Owned(address.to_string()), ports),
            Grant::Host { member, ports } => {
                (member, Cow::Borrowed(Destination::SHARDS_HOST), ports)
            }
            Grant::Publish { .. } => continue,
        };
        by_member.push((
            member.kind,
            member.name.as_str(),
            texts(&host, ports.first, ports.last),
        ));
        by_host.push((host, ports.first, ports.last));
    }
    let mut bytes = LIST_HEAD.saturating_mul(2);
    // Each member's ranges one run once sorted, its head counted once.
    by_member.sort_unstable_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)));
    let mut member: Option<(MemberKind, &str)> = None;
    for (kind, name, its) in by_member {
        if member != Some((kind, name)) {
            let length = u64::try_from(name.len()).unwrap_or(u64::MAX);
            bytes = bytes.saturating_add(MEMBER_HEAD).saturating_add(length);
            member = Some((kind, name));
        }
        bytes = bytes.saturating_add(its);
    }
    // Each host's ranges one run once sorted: ranges that meet or touch are one run of its ports, each kept once.
    by_host.sort_unstable();
    let mut run: Option<(Cow<'_, str>, u16, u16)> = None;
    for (host, first, last) in by_host {
        run = match run {
            Some((at, from, to))
                if at == host && u32::from(first) <= u32::from(to).saturating_add(1) =>
            {
                Some((at, from, to.max(last)))
            }
            Some((at, from, to)) => {
                bytes = bytes.saturating_add(texts(&at, from, to));
                Some((host, first, last))
            }
            None => Some((host, first, last)),
        };
    }
    if let Some((at, from, to)) = run {
        bytes = bytes.saturating_add(texts(&at, from, to));
    }
    Ok(bytes)
}

/// The bytes of the destinations `<host>:<port>` for each port `first` to `last` (`first` at or below `last`, as
/// `check_grants` holds them), each as D231's channel writes a text: a u32 length, then the host, a colon and the
/// port's decimal digits.
fn texts(host: &str, first: u16, last: u16) -> u64 {
    let ports = u64::from(last)
        .saturating_sub(u64::from(first))
        .saturating_add(1);
    let each = u64::try_from(host.len())
        .unwrap_or(u64::MAX)
        .saturating_add(DESTINATION_HEAD);
    // The range's ports of each decimal width: 1 to 9, 10 to 99, 100 to 999, 1000 to 9999, and 10000 up.
    let digits = [
        (1, 9),
        (10, 99),
        (100, 999),
        (1_000, 9_999),
        (10_000, u16::MAX),
    ]
    .into_iter()
    .zip(1u64..)
    .map(|((low, high), width): ((u16, u16), u64)| {
        let (from, to) = (first.max(low), last.min(high));
        if from > to {
            return 0;
        }
        u64::from(to)
            .saturating_sub(u64::from(from))
            .saturating_add(1)
            .saturating_mul(width)
    })
    .fold(0, u64::saturating_add);
    ports.saturating_mul(each).saturating_add(digits)
}

/// The destinations `grants` name by TCP, each once and sorted, and each member's, the members sorted by kind and
/// name: a grant's every port, as D236 writes a destination (`<name>:<port>`, `<a.b.c.d>:<port>`,
/// `host.docker.internal:<port>`). Bounded by its input, refused `Exhausted` past what memory gives (N7).
fn reach(grants: &[Grant]) -> Result<(Vec<String>, Vec<MemberReach>), Refusal> {
    let exhausted = |bound: usize| Refusal::Exhausted {
        what: "destinations",
        bound,
    };
    let mut each = Vec::new();
    for grant in grants {
        let (member, host, ports) = match grant {
            Grant::Egress {
                member,
                to: Destination::Name(name),
                ports,
            } => (member, Cow::Borrowed(name.as_str()), ports),
            Grant::Egress {
                member,
                to: Destination::Address(address),
                ports,
            } => (member, Cow::Owned(address.to_string()), ports),
            Grant::Host { member, ports } => {
                (member, Cow::Borrowed(Destination::SHARDS_HOST), ports)
            }
            Grant::Publish { .. } => continue,
        };
        for port in ports.first..=ports.last {
            each.try_reserve(1).map_err(|_| exhausted(each.len()))?;
            each.push((member.kind, member.name.as_str(), format!("{host}:{port}")));
        }
    }
    each.sort_unstable();
    each.dedup();
    let mut kept: Vec<String> = Vec::new();
    kept.try_reserve_exact(each.len())
        .map_err(|_| exhausted(each.len()))?;
    kept.extend(each.iter().map(|(_, _, at)| at.clone()));
    kept.sort_unstable();
    kept.dedup();
    let mut members: Vec<MemberReach> = Vec::new();
    for (kind, name, at) in each {
        let kind = shards_kind(kind);
        match members.last_mut() {
            Some(last) if last.kind == kind && last.name == name => {
                last.destinations
                    .try_reserve(1)
                    .map_err(|_| exhausted(last.destinations.len()))?;
                last.destinations.push(at);
            }
            _ => {
                members
                    .try_reserve(1)
                    .map_err(|_| exhausted(members.len()))?;
                members.push(MemberReach {
                    kind,
                    name: name.to_owned(),
                    destinations: vec![at],
                });
            }
        }
    }
    Ok((kept, members))
}

/// A booted VM's publishes as its run reports them, held to its grants (N6): each publish grant's member is one its
/// image runs; the host bound each publish grant's port once, where the grant names it, and nothing else; and each
/// such port reaches the grant's member alone, as Shards names its receivers (D231's `Bound::to`). Returns each as
/// bound, its receiver that member at its own port. A grant naming a member the image doesn't run, or a port the image
/// gives anyone else (another member, or none, so the run's own command), is the spec's fault (`Invalid`); a socket
/// bound past or short of the grants, or a receiver its image doesn't declare, Shards' (`Broken`).
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
        let kind = shards_kind(member.kind);
        let Some(runs) = members
            .iter()
            .find(|m| m.kind == kind && m.name == member.name && m.runs)
        else {
            return Err(Refusal::Invalid {
                what: "grants",
                why: format!("grants[{at}].member {member} isn't a member its image runs"),
            });
        };
        let proto = shards_proto(*proto);
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
        let receiver = match socket.to.as_slice() {
            [only] if only.member == runs.index => Receiver::Named { port: only.port },
            to => {
                return Err(Refusal::Invalid {
                    what: "grants",
                    why: format!(
                        "grants[{at}] publishes {member}'s port {port}, which its image gives {}",
                        whom(to, members)?
                    ),
                });
            }
        };
        made.push(Published {
            grant: u32::try_from(at).map_err(|_| room("grants", at))?,
            at: socket.host,
            receiver,
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

/// Whom a published port reaches, as Shards names its receivers (`to`), in a refusal's words: its members by kind and
/// name, or the run's own command where it names none (D122). A receiver no member of the image is is Shards' fault.
fn whom(to: &[Receiving], members: &[MemberInfo]) -> Result<String, Refusal> {
    if to.is_empty() {
        return Ok("no member: it reaches the run's own command".to_owned());
    }
    let mut named = Vec::new();
    named
        .try_reserve_exact(to.len())
        .map_err(|_| Refusal::Exhausted {
            what: "published port receivers",
            bound: to.len(),
        })?;
    for receiving in to {
        let Some(member) = members.iter().find(|m| m.index == receiving.member) else {
            return Err(Refusal::Broken(format!(
                "Shards named member {} as a published port's receiver, which its image doesn't declare",
                receiving.member
            )));
        };
        let kind = match member.kind {
            shards_api::image::MemberKind::Agent => "agent",
            shards_api::image::MemberKind::Harness => "harness",
        };
        named.push(format!(
            "{kind} {} at its port {}",
            member.name, receiving.port
        ));
    }
    Ok(named.join(" and "))
}

fn shards_kind(kind: MemberKind) -> shards_api::image::MemberKind {
    match kind {
        MemberKind::Agent => shards_api::image::MemberKind::Agent,
        MemberKind::Harness => shards_api::image::MemberKind::Harness,
    }
}

fn shards_proto(proto: Proto) -> shards_api::image::Proto {
    match proto {
        Proto::Tcp => shards_api::image::Proto::Tcp,
        Proto::Udp => shards_api::image::Proto::Udp,
    }
}

#[cfg(test)]
mod tests;
