//! Network grants for a microVM's members (joint plan step 15; CP-T7.7; docs/design/network-grants.md): what each
//! member of a box's image may reach past its VM, and what the VM's host publishes into it. Default deny: a flow no
//! grant names does not exist. A grant's variant is its way, and each names who, to whom and which ports (N1).
//! [`check_grants`] is the one check, run wherever grants enter, so each point refuses alike (N5).

use std::fmt;
use std::net::{IpAddr, SocketAddr};

use serde::{Deserialize, Serialize};

use crate::{Refusal, Upstream};

/// Which of its image's Agentfile declarations a member is: an `AGENT` or a `HARNESS`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MemberKind {
    Agent,
    Harness,
}

/// Who a grant is for: a member of the box's image, by the kind and name its Agentfile gives it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Member {
    pub kind: MemberKind,
    pub name: String,
}

impl Member {
    /// Longest name: Linux's `NAME_MAX` (include/uapi/linux/limits.h), a member's name being one directory of its
    /// image (`/harness/<name>`).
    pub const MAX_NAME: usize = 255;

    /// Whether its name is one Shards' Agentfile parser takes, BuildKit's stage name `^[a-z][a-z0-9._-]*$`
    /// (`valid_stage_name`), within [`Member::MAX_NAME`].
    fn check(&self, at: usize) -> Result<(), Refusal> {
        self.check_named(&format!("grants[{at}].member"))
            .map_err(invalid)
    }

    /// [`Member::check`]'s rule, its refusal naming the field `field` it was read from.
    pub(crate) fn check_named(&self, field: &str) -> Result<(), String> {
        let name = &self.name;
        if name.is_empty() || name.len() > Self::MAX_NAME {
            return Err(format!(
                "{field}.name {name:?} isn't 1 to {} bytes",
                Self::MAX_NAME
            ));
        }
        if !name.starts_with(|c: char| c.is_ascii_lowercase()) {
            return Err(format!(
                "{field}.name {name:?} doesn't start with a lowercase letter"
            ));
        }
        if let Some(c) = name.chars().find(|c| {
            !(c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '.' | '_' | '-'))
        }) {
            return Err(format!(
                "{field}.name {name:?} holds {c:?}: a member's name is lowercase letters, digits, \
                 '.', '_' and '-'"
            ));
        }
        Ok(())
    }
}

impl fmt::Display for Member {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind = match self.kind {
            MemberKind::Agent => "agent",
            MemberKind::Harness => "harness",
        };
        write!(f, "{kind} {}", self.name)
    }
}

/// A flow's transport protocol: the flows a VM's network process carries (Shards D31).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Proto {
    Tcp,
    Udp,
}

impl fmt::Display for Proto {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Tcp => "tcp",
            Self::Udp => "udp",
        })
    }
}

/// One protocol's ports, `first` to `last` inclusive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Ports {
    pub proto: Proto,
    pub first: u16,
    pub last: u16,
}

impl Ports {
    /// Whether it names a port: never port 0, which RFC 6335 §6 reserves, and `first` at or below `last`.
    fn check(&self, at: usize) -> Result<(), Refusal> {
        if self.first == 0 {
            return Err(invalid(format!(
                "grants[{at}].ports.first is 0, which RFC 6335 reserves"
            )));
        }
        if self.first > self.last {
            return Err(invalid(format!(
                "grants[{at}].ports run from {} down to {}",
                self.first, self.last
            )));
        }
        Ok(())
    }
}

/// To whom an egress grant reaches.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Destination {
    /// One fully qualified host name, lowercase, with no trailing dot or pattern. The VM's enforcer resolves it itself
    /// and admits flows only to the addresses it learned, never trusting the guest's resolver (N2).
    Name(String),
    /// One unicast address past the VM.
    Address(IpAddr),
}

impl Destination {
    /// Longest name in text form: RFC 1035 §2.3.4's 255 octets on the wire, less the first label's length octet and
    /// the root's.
    pub const MAX_NAME: usize = 253;
    /// Longest label: RFC 1035 §2.3.4.
    pub const MAX_LABEL: usize = 63;
    /// The name Shards' destinations give the VM's own host (D236, Docker Desktop's name for it), which its network
    /// process takes to the host's loopback: the host is a [`Grant::Host`]'s alone, so no egress names it.
    pub const SHARDS_HOST: &str = "host.docker.internal";

    fn check(&self, at: usize) -> Result<(), Refusal> {
        match self {
            Self::Name(name) => check_name(at, name),
            Self::Address(address) => match special(*address) {
                Some(why) => Err(invalid(format!(
                    "grants[{at}].to address {address} {why}, so it names no peer past the VM"
                ))),
                None => Ok(()),
            },
        }
    }
}

impl fmt::Display for Destination {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Name(name) => f.write_str(name),
            Self::Address(address) => write!(f, "{address}"),
        }
    }
}

/// One grant. Its variant is its way (N1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Grant {
    /// Out: `member` opens flows to `to` on `ports`. Their replies come back, and nothing else does.
    Egress {
        member: Member,
        to: Destination,
        ports: Ports,
    },
    /// Out to the VM's own host: `member` opens flows to a service the host serves on its loopback, on `ports`
    /// (kiln's endpoint where kiln runs beside the VM). No egress grant reaches the host itself.
    Host { member: Member, ports: Ports },
    /// In: the host publishes `member`'s `port` at its own address `host`, on `host_port`, or on a port the host's
    /// OS picks when that is none, recorded once bound. Whoever reaches that address connects, to `member` alone.
    Publish {
        member: Member,
        proto: Proto,
        port: u16,
        host: IpAddr,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        host_port: Option<u16>,
    },
}

impl Grant {
    /// Who the grant is for.
    pub fn member(&self) -> &Member {
        match self {
            Self::Egress { member, .. }
            | Self::Host { member, .. }
            | Self::Publish { member, .. } => member,
        }
    }

    fn check(&self, at: usize) -> Result<(), Refusal> {
        self.member().check(at)?;
        match self {
            Self::Egress { to, ports, .. } => {
                to.check(at)?;
                ports.check(at)
            }
            Self::Host { ports, .. } => ports.check(at),
            Self::Publish {
                port,
                host,
                host_port,
                ..
            } => {
                if *port == 0 {
                    return Err(invalid(format!(
                        "grants[{at}].port is 0, which RFC 6335 reserves"
                    )));
                }
                if *host_port == Some(0) {
                    return Err(invalid(format!(
                        "grants[{at}].host_port is 0: leave it out for a port the host picks"
                    )));
                }
                if host.is_unspecified() {
                    return Err(invalid(format!(
                        "grants[{at}].host {host} is every address, which names no one: name the host's address \
                         to publish at"
                    )));
                }
                // The host's loopback is where its own processes reach it, kiln's among them.
                let loopback = match host {
                    IpAddr::V4(v4) => v4.is_loopback(),
                    IpAddr::V6(v6) => v6.is_loopback(),
                };
                match special(*host) {
                    Some(why) if !loopback => Err(invalid(format!(
                        "grants[{at}].host {host} {why}, so the host can't publish at it"
                    ))),
                    _ => Ok(()),
                }
            }
        }
    }
}

/// A publish grant's port as the VM's boot bound it on the host (N6): what kiln connects to, reported by inspect
/// while the VM runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Published {
    /// The publish grant it serves, by its place in the box's grants.
    pub grant: u32,
    /// The host socket it is bound at: the grant's host address, on its host port or the one the OS picked.
    pub at: SocketAddr,
    /// Who receives what connects there.
    pub receiver: Receiver,
}

/// Who receives a published port's flows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Receiver {
    /// The member the image's Agentfile gives the port coming in (Shards D122), unverified: Shards 482c5d78's image
    /// boot named no receiver. No run reports it since Shards names one ([`Receiver::Named`]); its tag stays, the
    /// wire being append-only.
    Declared,
    /// The grant's member, which Shards' image boot names as the port's one receiver (D231's `Bound::to`), at the
    /// member's own port there: the published port, or the one an `EXPOSE` maps it to (D122). A run whose port
    /// reaches another member too, or none (its own command), is stopped as its boot returns (N6).
    Named { port: u16 },
}

/// Whether every grant is well formed and no flow is named twice, refused by the field that fails, never dropped or
/// half-applied (CLAUDE.md §3). Bounded by its input: one fallibly reserved working list, sorted (N7).
pub fn check_grants(grants: &[Grant]) -> Result<(), Refusal> {
    for (at, grant) in grants.iter().enumerate() {
        grant.check(at)?;
    }
    check_ranges(grants)?;
    check_publishes(grants)
}

/// Whether each published port is one of `grants`' publishes, bound where it names, and no grant bound twice
/// (N6): a report of the run is held to the grants it reports, never read past them.
pub fn check_published(grants: &[Grant], published: &[Published]) -> Result<(), Refusal> {
    let mut seen: Vec<u32> = working(published.len())?;
    for (at, port) in published.iter().enumerate() {
        let refused = |why: String| {
            Err(Refusal::Invalid {
                what: "published",
                why: format!("published[{at}] {why}"),
            })
        };
        let grant = usize::try_from(port.grant)
            .ok()
            .and_then(|grant| grants.get(grant));
        let Some(Grant::Publish {
            host, host_port, ..
        }) = grant
        else {
            return refused(format!("names grants[{}], which is no publish", port.grant));
        };
        if port.at.ip() != *host || host_port.is_some_and(|wanted| wanted != port.at.port()) {
            return refused(format!(
                "is bound at {}, not where grants[{}] publishes it",
                port.at, port.grant
            ));
        }
        seen.push(port.grant);
    }
    seen.sort_unstable();
    if seen.windows(2).any(|pair| matches!(pair, [a, b] if a == b)) {
        return Err(Refusal::Invalid {
            what: "published",
            why: "names one grant twice".to_owned(),
        });
    }
    Ok(())
}

/// Why `address` names nothing a grant may reach past the VM (RFC 6890's special purposes), if it doesn't.
fn special(address: IpAddr) -> Option<&'static str> {
    match address {
        IpAddr::V4(v4) => {
            let [first, ..] = v4.octets();
            if first == 0 {
                Some("is in 0.0.0.0/8, this host on this network")
            } else if v4.is_loopback() {
                Some("is loopback")
            } else if v4.is_link_local() {
                Some("is link-local")
            } else if v4.is_multicast() {
                Some("is multicast")
            } else if v4.is_broadcast() {
                Some("is the limited broadcast")
            } else {
                None
            }
        }
        IpAddr::V6(v6) => {
            if v6.is_unspecified() {
                Some("is unspecified")
            } else if v6.is_loopback() {
                Some("is loopback")
            } else if v6.is_unicast_link_local() {
                Some("is link-local")
            } else if v6.is_multicast() {
                Some("is multicast")
            } else if v6.to_ipv4_mapped().is_some() {
                Some("is IPv4-mapped (RFC 4291 §2.5.5.2): an IPv4 address is written as one")
            } else {
                None
            }
        }
    }
}

/// A host name as RFC 1123 §2.1 writes one, in one spelling: lowercase (RFC 4343 compares names case-insensitively),
/// no trailing dot, no pattern; more than one label, since a single label is completed from the host's search list
/// (RFC 1535); no all-numeric last label (RFC 3696 §2), which reads as an address; and nothing under `localhost`,
/// which resolves to loopback (RFC 6761 §6.3).
fn check_name(at: usize, name: &str) -> Result<(), Refusal> {
    let refused = |why: String| Err(invalid(format!("grants[{at}].to name {name:?} {why}")));
    if name.is_empty() || name.len() > Destination::MAX_NAME {
        return refused(format!("isn't 1 to {} characters", Destination::MAX_NAME));
    }
    if name.ends_with('.') {
        return refused("ends with the root's dot: write the name without it".to_owned());
    }
    for label in name.split('.') {
        if label.is_empty() || label.len() > Destination::MAX_LABEL {
            return refused(format!(
                "has a label of {} octets, not 1 to {}",
                label.len(),
                Destination::MAX_LABEL
            ));
        }
        if label.contains('*') {
            return refused("is a pattern, which names no one host".to_owned());
        }
        if let Some(c) = label
            .chars()
            .find(|c| !(c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-'))
        {
            return refused(format!(
                "holds {c:?}: a host name is lowercase letters, digits and hyphens"
            ));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return refused(format!(
                "has the label {label:?}, which starts or ends with a hyphen"
            ));
        }
    }
    if !name.contains('.') {
        return refused(
            "has one label, which the host's resolver completes from its search list".to_owned(),
        );
    }
    if name
        .rsplit('.')
        .next()
        .is_some_and(|last| last.bytes().all(|b| b.is_ascii_digit()))
    {
        return refused(
            "ends in an all-numeric label, which reads as an address: name an address as one"
                .to_owned(),
        );
    }
    if name.ends_with(".localhost") {
        return refused(
            "is under localhost, which resolves to loopback: the VM's host is a host grant"
                .to_owned(),
        );
    }
    if name == Destination::SHARDS_HOST {
        return refused(
            "is the VM's own host, as Shards names it (D236): the VM's host is a host grant"
                .to_owned(),
        );
    }
    Ok(())
}

/// What Shards owes Hex before an image's boot can hold `grants` exactly (N4), if anything: the first grant Shards'
/// destinations (D236: TCP to a host's name, an IPv4 address or the host itself) can't express at all awaits its own
/// capability, UDP to a name ([`Upstream::MemberGrantsByName`]), to an address, or to an IPv6 address by TCP
/// ([`Upstream::MemberGrantsByAddress`]), and the host's service by UDP ([`Upstream::MemberHostService`]). Every other
/// grant is held: a destination by TCP kept for its own members alone, which the run host holds every member of the
/// image to before the VM starts (Shards' D246), and a publish to the receiver Shards names (N6).
pub fn unheld(grants: &[Grant]) -> Option<Upstream> {
    grants.iter().find_map(|grant| match grant {
        Grant::Egress {
            to: Destination::Name(_),
            ports,
            ..
        } if ports.proto == Proto::Udp => Some(Upstream::MemberGrantsByName),
        Grant::Egress {
            to: Destination::Address(address),
            ports,
            ..
        } if address.is_ipv6() || ports.proto == Proto::Udp => {
            Some(Upstream::MemberGrantsByAddress)
        }
        Grant::Host { ports, .. } if ports.proto == Proto::Udp => Some(Upstream::MemberHostService),
        Grant::Egress { .. } | Grant::Host { .. } | Grant::Publish { .. } => None,
    })
}

/// One member's egress to one destination, or its reach to the host, of one protocol: grants of one key whose ports
/// meet name one flow twice.
#[derive(PartialEq, Eq, PartialOrd, Ord)]
struct RangeKey<'a> {
    member: &'a Member,
    to: Option<&'a Destination>,
    proto: Proto,
}

fn check_ranges(grants: &[Grant]) -> Result<(), Refusal> {
    let mut keyed: Vec<(RangeKey<'_>, u16, u16, usize)> = working(grants.len())?;
    for (at, grant) in grants.iter().enumerate() {
        let (key, ports) = match grant {
            Grant::Egress { member, to, ports } => (
                RangeKey {
                    member,
                    to: Some(to),
                    proto: ports.proto,
                },
                ports,
            ),
            Grant::Host { member, ports } => (
                RangeKey {
                    member,
                    to: None,
                    proto: ports.proto,
                },
                ports,
            ),
            Grant::Publish { .. } => continue,
        };
        keyed.push((key, ports.first, ports.last, at));
    }
    // Sorted by key and first port, any two ranges of a key that meet include a neighbouring pair that does: the
    // earlier range's successor starts between its start and the later one's, so within its end.
    keyed.sort_unstable();
    for pair in keyed.windows(2) {
        if let [(key, _, last, a), (next, first, _, b)] = pair
            && key == next
            && first <= last
        {
            let to = key
                .to
                .map_or_else(|| "its host".to_owned(), |to| to.to_string());
            return Err(invalid(format!(
                "grants[{a}] and grants[{b}] both let {} reach {to} on {}/{first}: each flow is one grant's",
                key.member, key.proto
            )));
        }
    }
    Ok(())
}

/// A published port has one receiver (Shards D122), and a host socket one published port behind it.
fn check_publishes(grants: &[Grant]) -> Result<(), Refusal> {
    type Receiver<'a> = (Proto, u16, &'a Member, IpAddr, Option<u16>, usize);
    let mut receivers: Vec<Receiver<'_>> = working(grants.len())?;
    let mut sockets: Vec<(Proto, IpAddr, u16, usize)> = working(grants.len())?;
    for (at, grant) in grants.iter().enumerate() {
        if let Grant::Publish {
            member,
            proto,
            port,
            host,
            host_port,
        } = grant
        {
            receivers.push((*proto, *port, member, *host, *host_port, at));
            if let Some(host_port) = host_port {
                sockets.push((*proto, *host, *host_port, at));
            }
        }
    }
    receivers.sort_unstable();
    for pair in receivers.windows(2) {
        if let [
            (proto, port, member, host, host_port, a),
            (next_proto, next_port, other, at_host, at_port, b),
        ] = pair
            && (proto, port) == (next_proto, next_port)
        {
            if member != other {
                return Err(invalid(format!(
                    "grants[{a}] and grants[{b}] publish {proto}/{port} to {member} and {other}: a published port \
                     has one receiver"
                )));
            }
            if (host, host_port) == (at_host, at_port) {
                return Err(invalid(format!(
                    "grants[{a}] and grants[{b}] publish {member}'s {proto}/{port} at {host} alike"
                )));
            }
        }
    }
    sockets.sort_unstable();
    for pair in sockets.windows(2) {
        if let [
            (proto, host, port, a),
            (next_proto, next_host, next_port, b),
        ] = pair
            && (proto, host, port) == (next_proto, next_host, next_port)
        {
            return Err(invalid(format!(
                "grants[{a}] and grants[{b}] both publish at {host} {proto}/{port}: a host socket serves one port"
            )));
        }
    }
    Ok(())
}

/// The check's working list: room for one entry per grant, reserved fallibly (N7).
fn working<T>(count: usize) -> Result<Vec<T>, Refusal> {
    let mut list = Vec::new();
    list.try_reserve_exact(count)
        .map_err(|_| Refusal::Exhausted {
            what: "grants",
            bound: count,
        })?;
    Ok(list)
}

fn invalid(why: String) -> Refusal {
    Refusal::Invalid {
        what: "grants",
        why,
    }
}

#[cfg(test)]
mod tests;
