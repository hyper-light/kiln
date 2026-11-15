//! A session's live overrides (kiln plan 01a1dd2e…, P6b.4; the person, 2026-11-15: "the ability to expose ports
//! (ingress and egress), connect/disconnect harnesses and agents and mcps from networks … these options should *only*
//! show if the session uses MicroVM runners", and "Add volumes"): what a person changes of a running session, in place
//! (the person, through ROOT: "without having to force the user to re-build the agentfile or MicroVM"). No Agentfile
//! edit, no image build, no new microVM and no session restart.
//!
//! - **What.** Ports published into its microVM (a host address and port to a member's port, TCP or UDP), egress
//!   granted out of it (a member to a host and port), its members on named networks, and volumes: a folder of its
//!   machine, read-only or read-write, at a path, for some of its members. Every grant names who, to whom, which ports
//!   and which way (default deny): nothing is implied.
//! - **Where each applies.** Ports, egress and networks are a microVM's: a session on its machine's host (engine
//!   `host`, P2.3) refuses them, naming its engine, and `agents.get` offers them only on a `microvm` one ([`ops_for`]).
//!   A volume is every engine's: its folder joins its harness's own allowed folders (each harness's own mechanism,
//!   [`Taking`]) and its session's scope (what kiln holds its file calls to, `super::hold::verdict`, where a read-only
//!   volume's changes are refused); in a microVM it's plugged in too, as asked (`kiln_seams::sandbox::Change::Plug`,
//!   through Hex).
//! - **Kept, and given again.** The session's state holds them ([`Overrides`], `State::overrides`) with how each place
//!   took each ([`Took`]); its harness's every start takes them again (a restart, a resume, a restore: [`dirs`] for its
//!   launch, [`changes`] for its microVM once it runs), for the session's life. Keeping them for an agent's later
//!   sessions (its definition) is a later step's.

use http::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ops::{Cx, Fail, Input, Op, Out, path, text};

/// A member of a session (Shards' D231 members): its image's HARNESS (the session's harness), an AGENT or an MCP
/// server, by name.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "LiveMember"))]
#[serde(tag = "kind", content = "name", rename_all = "snake_case")]
pub enum Member {
    Harness(String),
    Agent(String),
    Mcp(String),
}

impl Member {
    /// `said` as an operation takes a member: `harness` (the session's own, of kind `harness`), `agent:<name>` or
    /// `mcp:<name>`; none, its harness.
    pub fn read(said: Option<&str>, harness: &str) -> Result<Member, String> {
        let said = said.map(str::trim).filter(|s| !s.is_empty()).unwrap_or("harness");
        match said.split_once(':') {
            None if said == "harness" => Ok(Member::Harness(harness.to_owned())),
            Some(("harness", h)) if h == harness => Ok(Member::Harness(h.to_owned())),
            Some(("harness", h)) => Err(format!("its harness is {harness}, not {h}")),
            Some(("agent", n)) if named(n) => Ok(Member::Agent(n.to_owned())),
            Some(("mcp", n)) if named(n) => Ok(Member::Mcp(n.to_owned())),
            _ => Err(format!("{said:?} is no member: `harness`, `agent:<name>` or `mcp:<name>`")),
        }
    }

    /// As an operation takes it ([`Member::read`]).
    pub fn said(&self) -> String {
        match self {
            Member::Harness(_) => "harness".to_owned(),
            Member::Agent(n) => format!("agent:{n}"),
            Member::Mcp(n) => format!("mcp:{n}"),
        }
    }
}

/// A member's or a network's name: letters, digits, `-`, `_` and `.`, as an Agentfile names its members.
fn named(n: &str) -> bool {
    !n.is_empty() && n.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// A port's protocol.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "LiveProtocol"))]
#[serde(rename_all = "lowercase")]
pub enum Protocol {
    Tcp,
    Udp,
}

/// Whether a volume's members may change it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, schemars::JsonSchema)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "LiveAccess"))]
#[serde(rename_all = "lowercase")]
pub enum Access {
    /// Read-only.
    Ro,
    /// Read-write.
    Rw,
}

/// A port published into its microVM: the host's address and port, which reach its `guest_port` there, and so the
/// members its image gives that port coming in (Shards' `ImageGuest::publish`, D269; D122).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "LivePort"))]
pub struct Port {
    /// The host address it's published on: who may reach it (`127.0.0.1`: this machine alone).
    pub bind: String,
    pub host_port: u16,
    pub guest_port: u16,
    pub protocol: Protocol,
}

/// What names a published port: the host's address, port and protocol, which one publication holds.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "LivePortKey"))]
pub struct PortKey {
    pub bind: String,
    pub host_port: u16,
    pub protocol: Protocol,
}

impl Port {
    fn key(&self) -> PortKey {
        PortKey { bind: self.bind.clone(), host_port: self.host_port, protocol: self.protocol }
    }
}

/// Egress granted out of its microVM: `member` to `host` (a DNS name or an address) on `port`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "LiveEgress"))]
pub struct Egress {
    pub member: Member,
    pub host: String,
    pub port: u16,
}

/// A member on a named network, shared with the runs connected to it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "LiveNetwork"))]
pub struct Network {
    pub member: Member,
    pub name: String,
}

/// A folder of the session's machine, given to some of its members.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "LiveVolume"))]
pub struct Volume {
    /// The folder, on the session's machine.
    pub path: String,
    /// Where its members see it: in a microVM, its path there (as kiln plugs the machine's folders in,
    /// `sandbox::isolated::guest_path`, unless asked); on the host, the folder itself.
    pub at: String,
    pub access: Access,
    /// Who sees it: a host session's harness alone.
    pub members: Vec<Member>,
}

/// One change to a session's overrides, as `Command::Override` carries it to its machine.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "LiveChange"))]
#[serde(tag = "op", content = "of", rename_all = "snake_case")]
pub enum Change {
    Publish(Port),
    Unpublish(PortKey),
    Grant(Egress),
    Revoke(Egress),
    Connect(Network),
    Disconnect(Network),
    Add(Volume),
    /// The volume of this folder.
    Remove(String),
}

/// How a place took an override: applied now, from the harness's next turn or start, with nothing of its own to
/// change (kiln's scope holds it), or refused, each saying why.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "LiveTook"))]
#[serde(tag = "took", rename_all = "snake_case")]
pub enum Took {
    Now { how: String },
    NextTurn { how: String },
    NextStart { how: String },
    Nothing { why: String },
    Refused { why: String },
}

/// A port as its session holds it, and how its microVM took it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "LiveHeldPort"))]
pub struct HeldPort {
    pub port: Port,
    pub microvm: Took,
}

/// An egress grant as its session holds it, and how its microVM took it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "LiveHeldEgress"))]
pub struct HeldEgress {
    pub egress: Egress,
    pub microvm: Took,
}

/// A member's network as its session holds it, and how its microVM took it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "LiveHeldNetwork"))]
pub struct HeldNetwork {
    pub network: Network,
    pub microvm: Took,
}

/// A volume as its session holds it: how its harness took it among its allowed folders, how kiln's scope did, and
/// (a microVM's) how its microVM did.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "LiveHeldVolume"))]
pub struct HeldVolume {
    pub volume: Volume,
    pub harness: Took,
    pub scope: Took,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[cfg_attr(test, ts(optional))]
    pub microvm: Option<Took>,
}

/// What a person changed of a session while it runs ([module docs](self)): each, and how each place took it.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(test, derive(ts_rs::TS))]
#[cfg_attr(test, ts(rename = "LiveOverrides"))]
pub struct Overrides {
    /// The changes it took, counted.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub revision: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ports: Vec<HeldPort>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub egress: Vec<HeldEgress>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub networks: Vec<HeldNetwork>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<HeldVolume>,
    /// Where its microVM takes a folder live, as its launch declared them (Shards' `ImageSpec::plug_roots`, D271): the
    /// top-level directories of its project and its person's volumes, and its session's volume root
    /// ([`crate::sandbox::isolated::VOLUMES`]), a folder going strictly beneath one.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub roots: Vec<String>,
    /// The most egress its microVM may be granted live, as its launch declared it (Shards' `ImageSpec::egress_ceiling`;
    /// `[agents] egress_ceiling`), each `host:port`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub ceiling: Vec<String>,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

impl Overrides {
    pub fn is_empty(&self) -> bool {
        self.revision == 0 && self.ports.is_empty() && self.egress.is_empty() && self.networks.is_empty() && self.volumes.is_empty() && self.roots.is_empty() && self.ceiling.is_empty()
    }
}

/// Whether `change` fits what its microVM declared as it started (`o`'s [`Overrides::roots`] and
/// [`Overrides::ceiling`]): a folder strictly beneath one of its plug roots, as Shards plugs one in (D271; one elsewhere
/// is refused, `Outside`, until a member restarted can take it, D270); egress within its ceiling. Why not, in words. A
/// removal always fits.
pub fn fits(change: &Change, o: &Overrides) -> Result<(), String> {
    match change {
        Change::Add(v) if !o.roots.iter().any(|r| beneath(&v.at, r)) => Err(format!(
            "{} is outside every folder its microVM takes a plug in live ({}): give it beneath one; by default it goes beneath {}",
            v.at,
            if o.roots.is_empty() { "none declared".to_owned() } else { o.roots.join(", ") },
            crate::sandbox::isolated::VOLUMES
        )),
        Change::Grant(e) if !o.ceiling.iter().any(|c| within(c, &e.host, e.port)) => Err(format!(
            "{}:{} is past the most egress this session may be granted live ({}): its person's or org's policy names what may be (`[agents] egress_ceiling`)",
            e.host,
            e.port,
            if o.ceiling.is_empty() { "none".to_owned() } else { o.ceiling.join(", ") }
        )),
        _ => Ok(()),
    }
}

/// Whether path `at` is strictly beneath `root`, as Shards takes a plug (D271).
fn beneath(at: &str, root: &str) -> bool {
    let root = root.trim_end_matches('/');
    at.strip_prefix(root).is_some_and(|rest| rest.len() > 1 && rest.starts_with('/'))
}

/// Whether ceiling entry `entry` (`host:port`) names `host` on `port`, its host as DNS compares names (RFC 4343).
fn within(entry: &str, host: &str, port: u16) -> bool {
    entry.rsplit_once(':').is_some_and(|(h, p)| h.trim_start_matches('[').trim_end_matches(']').eq_ignore_ascii_case(host) && p.parse::<u16>().ok() == Some(port))
}

/// How a harness takes a session's folders among its allowed ones (each harness's own mechanism: `super::kinds::Adapter`'s
/// `dirs_changed`), and in its words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Taking {
    /// Now, as it runs.
    Now(String),
    /// From its next turn.
    NextTurn(String),
    /// Only as it starts: it starts again with them (on its machine's host, once nothing is under way, its conversation
    /// going on; in a microVM, as it next starts, kiln starting no new microVM for it).
    NextStart(String),
    /// It holds its tools to no folder of its own: nothing of it to change.
    Nothing(String),
}

/// One of a session's folders as its harness is told of it: where it sees it, and whether it may change it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dir {
    pub at: String,
    pub writable: bool,
}

/// The folders `o`'s volumes give its harness, as it sees them, in the order they were added.
pub fn dirs(o: &Overrides) -> Vec<Dir> {
    o.volumes.iter().filter(|v| v.volume.members.iter().any(|m| matches!(m, Member::Harness(_)))).map(|v| Dir { at: v.volume.at.clone(), writable: v.volume.access == Access::Rw }).collect()
}

/// Why a host session refuses a network change: its engine named.
pub fn host_refuses(what: &str) -> String {
    format!("{what} is a microVM session's: this session runs on its machine's host (engine host), where kiln publishes no ports, grants no egress and joins no networks. Start it on a runner whose engine is microvm.")
}

/// A port's or a host's checks before it's taken: every grant explicit (default deny).
fn checked_port(p: &Port) -> Result<(), String> {
    p.bind.trim().parse::<std::net::IpAddr>().map_err(|_| format!("{:?} is no address to publish on: an IPv4 or IPv6 address (`127.0.0.1`: this machine alone)", p.bind))?;
    if p.host_port == 0 || p.guest_port == 0 {
        return Err("a port is 1 to 65535: 0 names no port".to_owned());
    }
    Ok(())
}

/// Whether `host` is a host egress can name: an address, or a DNS name (RFC 1035 §2.3.4: labels of 1 to 63 letters,
/// digits and hyphens, neither starting nor ending with one, 253 octets in all).
fn host_named(host: &str) -> bool {
    if host.parse::<std::net::IpAddr>().is_ok() {
        return true;
    }
    let name = host.strip_suffix('.').unwrap_or(host);
    !name.is_empty()
        && name.len() <= 253
        && name.split('.').all(|l| !l.is_empty() && l.len() <= 63 && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') && !l.starts_with('-') && !l.ends_with('-'))
}

/// Whether `change` can be taken by a session whose engine `microvm` is (else its host's) and whose harness is
/// `harness`: why not, in words. The server asks before it sends one, and the session again before it takes it (a
/// command can come by its generic route too).
pub fn check(change: &Change, microvm: bool, harness: &str) -> Result<(), String> {
    let on_host = |what: &str| if microvm { Ok(()) } else { Err(host_refuses(what)) };
    let members = |m: &[Member]| -> Result<(), String> {
        if !microvm && m.iter().any(|m| !matches!(m, Member::Harness(_))) {
            return Err("a session on its machine's host has one member, its harness: agents and MCP servers are a microVM's members".to_owned());
        }
        match m.iter().find(|m| matches!(m, Member::Harness(h) if h != harness)) {
            Some(Member::Harness(h)) => Err(format!("its harness is {harness}, not {h}")),
            _ => Ok(()),
        }
    };
    match change {
        Change::Publish(p) => {
            on_host("Publishing a port")?;
            checked_port(p)
        }
        Change::Unpublish(_) => on_host("Unpublishing a port"),
        Change::Grant(e) | Change::Revoke(e) => {
            on_host("Egress")?;
            members(std::slice::from_ref(&e.member))?;
            if !host_named(&e.host) {
                return Err(format!("{:?} is no host egress can name: a DNS name or an address", e.host));
            }
            if e.port == 0 {
                return Err("a port is 1 to 65535: 0 names no port".to_owned());
            }
            Ok(())
        }
        Change::Connect(n) | Change::Disconnect(n) => {
            on_host("A network")?;
            members(std::slice::from_ref(&n.member))?;
            if named(&n.name) { Ok(()) } else { Err(format!("{:?} is no network's name: letters, digits, `-`, `_` and `.`", n.name)) }
        }
        Change::Add(v) => {
            if v.members.is_empty() {
                return Err("a volume is for one member at least".to_owned());
            }
            members(&v.members)?;
            if !absolute(&v.path) {
                return Err(format!("{:?} isn't a folder's absolute path on its machine", v.path));
            }
            if microvm && !v.at.starts_with('/') {
                return Err(format!("{:?} isn't an absolute path in its microVM", v.at));
            }
            if !microvm && v.at != v.path {
                return Err(format!("a session on its machine's host sees {} at its own path, not {}", v.path, v.at));
            }
            Ok(())
        }
        Change::Remove(p) => {
            if absolute(p) {
                Ok(())
            } else {
                Err(format!("{p:?} isn't a folder's absolute path on its machine"))
            }
        }
    }
}

/// A path that names a folder from the root: a Unix one, or a Windows one (`C:\…`, `\\server\…`).
fn absolute(p: &str) -> bool {
    p.starts_with('/') || p.starts_with(r"\\") || p.get(1..3).is_some_and(|s| s == ":\\" || s == ":/")
}

/// What a change did to the record ([`take`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Taken {
    /// It holds a new one.
    Added,
    /// It held one: let go of it (here, for its places to let go of too).
    Removed(Removed),
    /// It held that already, as asked: nothing changes.
    Same,
}

/// What a change took out of the record, for its places to let go of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Removed {
    Port(Port),
    Egress(Egress),
    Network(Network),
    Volume(Volume),
}

/// `change` taken into `o`, as each place has still to take it (`Took::NextStart` until it does): one held already
/// as asked changes nothing; one that names what's held otherwise (a port published elsewhere, a folder given at
/// another access) is refused, saying what it holds; a removal of what it doesn't hold is refused. Its revision
/// counts each it takes.
pub fn take(o: &mut Overrides, change: &Change) -> Result<Taken, String> {
    let waiting = || Took::NextStart { how: "once its harness runs".to_owned() };
    let taken = match change {
        Change::Publish(p) => match o.ports.iter().find(|h| h.port.key() == p.key()) {
            Some(h) if h.port == *p => Taken::Same,
            Some(h) => return Err(format!("{}:{}/{} is published already, to its port {}: unpublish it first", h.port.bind, h.port.host_port, proto(h.port.protocol), h.port.guest_port)),
            None => {
                o.ports.push(HeldPort { port: p.clone(), microvm: waiting() });
                Taken::Added
            }
        },
        Change::Unpublish(k) => match o.ports.iter().position(|h| h.port.key() == *k) {
            Some(i) => Taken::Removed(Removed::Port(o.ports.remove(i).port)),
            None => return Err(format!("{}:{}/{} isn't published", k.bind, k.host_port, proto(k.protocol))),
        },
        Change::Grant(e) => {
            if o.egress.iter().any(|h| h.egress == *e) {
                Taken::Same
            } else {
                o.egress.push(HeldEgress { egress: e.clone(), microvm: waiting() });
                Taken::Added
            }
        }
        Change::Revoke(e) => match o.egress.iter().position(|h| h.egress == *e) {
            Some(i) => Taken::Removed(Removed::Egress(o.egress.remove(i).egress)),
            None => return Err(format!("{} isn't granted {}:{}", e.member.said(), e.host, e.port)),
        },
        Change::Connect(n) => {
            if o.networks.iter().any(|h| h.network == *n) {
                Taken::Same
            } else {
                o.networks.push(HeldNetwork { network: n.clone(), microvm: waiting() });
                Taken::Added
            }
        }
        Change::Disconnect(n) => match o.networks.iter().position(|h| h.network == *n) {
            Some(i) => Taken::Removed(Removed::Network(o.networks.remove(i).network)),
            None => return Err(format!("{} isn't on network {}", n.member.said(), n.name)),
        },
        Change::Add(v) => match o.volumes.iter().find(|h| h.volume.path == v.path || h.volume.at == v.at) {
            Some(h) if h.volume == *v => Taken::Same,
            Some(h) => return Err(format!("{} is given already ({}, at {}): remove it first", h.volume.path, access(h.volume.access), h.volume.at)),
            None => {
                o.volumes.push(HeldVolume { volume: v.clone(), harness: waiting(), scope: waiting(), microvm: None });
                Taken::Added
            }
        },
        Change::Remove(p) => match o.volumes.iter().position(|h| h.volume.path == *p) {
            Some(i) => Taken::Removed(Removed::Volume(o.volumes.remove(i).volume)),
            None => return Err(format!("{p} isn't one of its volumes")),
        },
    };
    if taken != Taken::Same {
        o.revision = o.revision.saturating_add(1);
    }
    Ok(taken)
}

fn proto(p: Protocol) -> &'static str {
    match p {
        Protocol::Tcp => "tcp",
        Protocol::Udp => "udp",
    }
}

fn access(a: Access) -> &'static str {
    match a {
        Access::Ro => "read-only",
        Access::Rw => "read-write",
    }
}

/// What a microVM is asked to hold of `o` once its run starts (a restart, a resume, a restore: its harness's every
/// start), each with where its took goes: its ports, egress, networks and volumes, in the order they were taken.
pub fn changes(o: &Overrides) -> Vec<(Place, kiln_seams::sandbox::Change)> {
    let mut out = Vec::new();
    out.extend(o.ports.iter().enumerate().map(|(i, h)| (Place::Port(i), seam_publish(&h.port))));
    out.extend(o.egress.iter().enumerate().map(|(i, h)| (Place::Egress(i), seam_grant(&h.egress, true))));
    out.extend(o.networks.iter().enumerate().map(|(i, h)| (Place::Network(i), seam_network(&h.network, true))));
    out.extend(o.volumes.iter().enumerate().map(|(i, h)| (Place::Volume(i), seam_plug(&h.volume))));
    out
}

/// Which held override a took is for ([`changes`], [`set_took`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Port(usize),
    Egress(usize),
    Network(usize),
    Volume(usize),
}

/// How its microVM took the override at `place`.
pub fn set_took(o: &mut Overrides, place: Place, took: Took) {
    match place {
        Place::Port(i) => {
            if let Some(h) = o.ports.get_mut(i) {
                h.microvm = took;
            }
        }
        Place::Egress(i) => {
            if let Some(h) = o.egress.get_mut(i) {
                h.microvm = took;
            }
        }
        Place::Network(i) => {
            if let Some(h) = o.networks.get_mut(i) {
                h.microvm = took;
            }
        }
        Place::Volume(i) => {
            if let Some(h) = o.volumes.get_mut(i) {
                h.microvm = Some(took);
            }
        }
    }
}

/// The place of the override `change` added, once [`take`] took it.
pub fn place_of(o: &Overrides, change: &Change) -> Option<Place> {
    match change {
        Change::Publish(p) => o.ports.iter().position(|h| h.port == *p).map(Place::Port),
        Change::Grant(e) => o.egress.iter().position(|h| h.egress == *e).map(Place::Egress),
        Change::Connect(n) => o.networks.iter().position(|h| h.network == *n).map(Place::Network),
        Change::Add(v) => o.volumes.iter().position(|h| h.volume == *v).map(Place::Volume),
        Change::Unpublish(_) | Change::Revoke(_) | Change::Disconnect(_) | Change::Remove(_) => None,
    }
}

/// The seam's change that adds what `change` added ([`take`]'s `Added`), or lets go of what it removed (`removed`).
pub fn seam_change(change: &Change, removed: Option<&Removed>) -> Option<kiln_seams::sandbox::Change> {
    match (change, removed) {
        (Change::Publish(p), _) => Some(seam_publish(p)),
        (Change::Grant(e), _) => Some(seam_grant(e, true)),
        (Change::Connect(n), _) => Some(seam_network(n, true)),
        (Change::Add(v), _) => Some(seam_plug(v)),
        (_, Some(Removed::Port(p))) => Some(kiln_seams::sandbox::Change::Unpublish { udp: p.protocol == Protocol::Udp, host: bound(&p.bind, p.host_port) }),
        (_, Some(Removed::Egress(e))) => Some(seam_grant(e, false)),
        (_, Some(Removed::Network(n))) => Some(seam_network(n, false)),
        (_, Some(Removed::Volume(v))) => Some(kiln_seams::sandbox::Change::Unplug { at: v.at.clone() }),
        (_, None) => None,
    }
}

fn member_of(m: &Member) -> kiln_seams::sandbox::Member {
    match m {
        Member::Harness(n) | Member::Agent(n) | Member::Mcp(n) => kiln_seams::sandbox::Member(n.clone()),
    }
}

fn seam_publish(p: &Port) -> kiln_seams::sandbox::Change {
    kiln_seams::sandbox::Change::Publish { guest_port: p.guest_port, udp: p.protocol == Protocol::Udp, host: Some(p.bind.clone()), host_port: p.host_port }
}

/// `bind` and `port` as one address, as a host binds it (`127.0.0.1:8080`, `[::1]:8080`).
fn bound(bind: &str, port: u16) -> String {
    match bind.parse::<std::net::IpAddr>() {
        Ok(ip) => std::net::SocketAddr::new(ip, port).to_string(),
        Err(_) => format!("{bind}:{port}"),
    }
}

fn seam_grant(e: &Egress, grant: bool) -> kiln_seams::sandbox::Change {
    let (member, host, port) = (member_of(&e.member), e.host.clone(), e.port);
    if grant { kiln_seams::sandbox::Change::Grant { member, host, port } } else { kiln_seams::sandbox::Change::Revoke { member, host, port } }
}

fn seam_network(n: &Network, connect: bool) -> kiln_seams::sandbox::Change {
    let (member, network) = (member_of(&n.member), n.name.clone());
    if connect { kiln_seams::sandbox::Change::Connect { member, network } } else { kiln_seams::sandbox::Change::Disconnect { member, network } }
}

fn seam_plug(v: &Volume) -> kiln_seams::sandbox::Change {
    use kiln_seams::sandbox::{PlugFor, PlugSource};
    kiln_seams::sandbox::Change::Plug { source: PlugSource::Host(v.path.clone()), to: PlugFor::Members(v.members.iter().map(member_of).collect()), at: v.at.clone(), read_only: v.access == Access::Ro }
}

/// How its microVM took a change it was asked (`Run::change`): now, or refused in its words (one that takes none live,
/// as Hex's microVMs until its wire calls land, P6b.3, says so).
pub fn microvm_took(r: Result<(), kiln_seams::sandbox::Refusal>) -> Took {
    match r {
        Ok(()) => Took::Now { how: "its microVM took it live".to_owned() },
        Err(why) => Took::Refused { why: format!("its microVM didn't take it: {why}") },
    }
}

/// `taking` as the took of a volume's harness.
pub fn harness_took(taking: Taking) -> Took {
    match taking {
        Taking::Now(how) => Took::Now { how },
        Taking::NextTurn(how) => Took::NextTurn { how },
        Taking::NextStart(how) => Took::NextStart { how },
        Taking::Nothing(why) => Took::Nothing { why },
    }
}

/// What kiln's scope took of a volume: its file calls there are inside the session's folder, a read-only one's changes
/// refused (`super::hold::verdict`).
pub fn scope_took(v: &Volume) -> Took {
    let how = match v.access {
        Access::Ro => format!("kiln holds its file calls in {} to reading: a change there is refused", v.at),
        Access::Rw => format!("kiln lets its file calls in {} read and change it", v.at),
    };
    Took::Now { how }
}

/// The operations a session of engine `microvm` (else its host's) takes: its volumes' on every engine, and its ports',
/// egress' and networks' only in a microVM (the person: "these options should *only* show if the session uses MicroVM
/// runners").
pub fn ops_for(microvm: bool) -> Vec<&'static str> {
    let network = ["sessions.ports.publish", "sessions.ports.unpublish", "sessions.egress.grant", "sessions.egress.revoke", "sessions.networks.connect", "sessions.networks.disconnect"];
    let volumes = ["sessions.volumes.add", "sessions.volumes.remove"];
    if microvm { network.into_iter().chain(volumes).collect() } else { volumes.to_vec() }
}

/// What `agents.get` says of a session's live overrides: its engine, the operations it takes, and what it holds.
pub fn view(info: &super::session::Info, state: &Value) -> Value {
    let microvm = info.isolation.is_some();
    json!({
        "engine": if microvm { "microvm" } else { "host" },
        "ops": ops_for(microvm),
        "overrides": state.get("overrides").cloned().unwrap_or_else(|| json!({})),
    })
}

/// What `sessions.grants` says of session `info`, holding `state` (kiln plan 01a1dd2e…, P5.2's page: the base beside
/// the live overrides): its engine; its base, what its launch gave it as it started (a microVM session's as its runner
/// launched it, `Runs::Microvm::granted`; a host session's harness its working folder): its folder, the machine's
/// folders it sees and where, the folders its harness is allowed (its folder and theirs), where its microVM takes a
/// folder live and the most egress it may be granted live (as its launch declared them), what it reaches past its
/// microVM (`reach`; `{"as": "host"}` on its host, which narrows nothing), its credentials by reference, and no port
/// published or network joined (none is, at a launch: default deny); and its live overrides beside it.
pub fn grants_of(info: &super::session::Info, state: &Value) -> Value {
    let overrides = state.get("overrides").cloned().unwrap_or_else(|| json!({}));
    let declared = |k: &str| overrides.get(k).cloned().unwrap_or_else(|| json!([]));
    let mut live = overrides.clone();
    if let Some(o) = live.as_object_mut() {
        o.remove("roots");
        o.remove("ceiling");
    }
    let base = match &info.runs {
        Some(super::session::Runs::Microvm { image, reach, granted, .. }) => {
            let g = granted.as_deref().cloned().unwrap_or_default();
            let mut allowed: Vec<String> = g.folder.iter().cloned().collect();
            for v in &g.volumes {
                if !allowed.contains(&v.at) {
                    allowed.push(v.at.clone());
                }
            }
            json!({"image": image, "folder": g.folder, "volumes": g.volumes, "allowed_dirs": allowed, "plug_roots": declared("roots"), "egress": reach, "egress_ceiling": declared("ceiling"), "ports": [], "networks": [], "secrets": g.secrets, "reported": granted.is_some()})
        }
        _ => json!({"folder": info.cwd, "volumes": [], "allowed_dirs": [info.cwd], "plug_roots": [], "egress": {"as": "host"}, "egress_ceiling": [], "ports": [], "networks": [], "secrets": [], "reported": true}),
    };
    json!({"agent": info.id, "engine": if info.isolation.is_some() { "microvm" } else { "host" }, "base": base, "live": live})
}

// ---------------------------------------------------------------------------------------- the operations

/// `sessions.grants`: a session the caller sees, its base beside its live overrides ([`grants_of`]).
async fn grants(cx: Cx, v: Value) -> Out {
    let info = super::ops::known(&cx, &text(&v, "agent")?)?;
    super::lineage::may_see(&cx.hub, &cx.caller, &info.id).await?;
    let state = cx.hub.harness.state_value(&info.id).unwrap_or_default();
    Ok(grants_of(&info, &state))
}

/// The session `v` names, seen and steered by the caller (an agent steers only those it started, and theirs).
async fn session_of(cx: &Cx, v: &Value) -> Result<super::session::Info, Fail> {
    let id = text(v, "agent")?;
    let info = super::ops::known(cx, &id)?;
    super::lineage::may_steer(&cx.hub, &cx.caller, &info.id).await?;
    Ok(info)
}

/// Sends `change` to session `info`'s machine, checked first as its session checks it: taken, refused with its words,
/// or why it didn't get there.
async fn send(cx: &Cx, info: &super::session::Info, change: Change) -> Out {
    let microvm = info.isolation.is_some();
    check(&change, microvm, &info.kind).map_err(|why| (if !microvm && why.contains("(engine host)") { StatusCode::CONFLICT } else { StatusCode::BAD_REQUEST }, why))?;
    // What its microVM declared as it started, as the server last heard it: a folder outside its plug roots, or egress
    // past its ceiling, is refused here as its session would refuse it.
    if microvm {
        let held: Overrides = cx.hub.harness.state_value(&info.id).and_then(|s| s.get("overrides").cloned()).and_then(|o| serde_json::from_value(o).ok()).unwrap_or_default();
        fits(&change, &held).map_err(|why| (StatusCode::CONFLICT, why))?;
    }
    use super::registry::Sent;
    let asked = json!(change);
    match cx.hub.harness.command(&cx.hub, &info.id, super::model::Command::Override { change }).await {
        Sent::Ok | Sent::Taken | Sent::Accepted(_) | Sent::Published(_) => {}
        Sent::Parked => return Err((StatusCode::BAD_GATEWAY, format!("{}'s machine isn't connected right now: nothing changed", info.name))),
        Sent::NoSession => return Err((StatusCode::NOT_FOUND, format!("no agent {}", info.id))),
        Sent::Unreachable => return Err((StatusCode::BAD_GATEWAY, format!("{}'s machine isn't connected right now: nothing changed", info.name))),
        Sent::PublicationRefused(e) => return Err((StatusCode::CONFLICT, format!("{e:?}"))),
        Sent::Refused(r) => return Err((StatusCode::CONFLICT, format!("{r:?}"))),
        s @ (Sent::Rejected { .. } | Sent::Unanswered(_)) => return Err(s.failure().unwrap_or((StatusCode::CONFLICT, "its runner refused it".to_owned()))),
    }
    // Audited as its route is, one row (`crate::ops::call`).
    Ok(json!({"taken": true, "agent": info.id, "engine": if microvm { "microvm" } else { "host" }, "change": asked, "said": "its machine applies it in place: agents_get shows each override and how each place took it (state.overrides)"}))
}

fn opt<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn read<T: serde::de::DeserializeOwned>(v: &Value) -> Result<T, Fail> {
    serde_json::from_value(v.clone()).map_err(|e| (StatusCode::BAD_REQUEST, format!("{e}")))
}

async fn publish(cx: Cx, v: Value) -> Out {
    let info = session_of(&cx, &v).await?;
    let i: input::Publish = read(&v)?;
    send(&cx, &info, Change::Publish(Port { bind: i.bind.trim().to_owned(), host_port: i.host_port, guest_port: i.guest_port, protocol: i.protocol })).await
}

async fn unpublish(cx: Cx, v: Value) -> Out {
    let info = session_of(&cx, &v).await?;
    let i: input::Unpublish = read(&v)?;
    send(&cx, &info, Change::Unpublish(PortKey { bind: i.bind.trim().to_owned(), host_port: i.host_port, protocol: i.protocol })).await
}

async fn egress(cx: Cx, v: Value, grant: bool) -> Out {
    let info = session_of(&cx, &v).await?;
    let i: input::Egress = read(&v)?;
    let member = Member::read(i.member.as_deref(), &info.kind).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let e = Egress { member, host: i.host.trim().to_ascii_lowercase(), port: i.port };
    send(&cx, &info, if grant { Change::Grant(e) } else { Change::Revoke(e) }).await
}

async fn network(cx: Cx, v: Value, connect: bool) -> Out {
    let info = session_of(&cx, &v).await?;
    let i: input::Network = read(&v)?;
    let member = Member::read(i.member.as_deref(), &info.kind).map_err(|e| (StatusCode::BAD_REQUEST, e))?;
    let n = Network { member, name: i.network.trim().to_owned() };
    send(&cx, &info, if connect { Change::Connect(n) } else { Change::Disconnect(n) }).await
}

async fn add(cx: Cx, v: Value) -> Out {
    let info = session_of(&cx, &v).await?;
    let i: input::Add = read(&v)?;
    let path = i.path.trim().to_owned();
    let microvm = info.isolation.is_some();
    // In a microVM, beneath its session's volume root by default, which it takes live (`Overrides::roots`).
    let at = match opt(&v, "at") {
        Some(at) => at.to_owned(),
        None if microvm => crate::sandbox::isolated::volume_at(&path).map_err(|e| (StatusCode::BAD_REQUEST, e))?,
        None => path.clone(),
    };
    if !microvm && at != path {
        return Err((StatusCode::BAD_REQUEST, format!("a session on its machine's host sees {path} at its own path, not {at}")));
    }
    let members = match &i.members {
        None => vec![Member::Harness(info.kind.clone())],
        Some(said) => said.iter().map(|m| Member::read(Some(m), &info.kind)).collect::<Result<Vec<_>, _>>().map_err(|e| (StatusCode::BAD_REQUEST, e))?,
    };
    send(&cx, &info, Change::Add(Volume { path, at, access: i.access, members })).await
}

async fn remove(cx: Cx, v: Value) -> Out {
    let info = session_of(&cx, &v).await?;
    let i: input::Remove = read(&v)?;
    send(&cx, &info, Change::Remove(i.path.trim().to_owned())).await
}

/// A route under a session's: `POST /api/harness/sessions/:agent/<parts…>`.
fn route_of(v: &Value, parts: &[&str]) -> Result<(Method, Vec<String>), Fail> {
    let agent = text(v, "agent")?;
    let mut segs = path(&["api", "harness", "sessions", &agent]);
    segs.extend(parts.iter().map(|s| (*s).to_owned()));
    Ok((Method::POST, segs))
}

/// The `sessions.*` operations of a session's live overrides.
pub fn list_ops() -> Vec<Op> {
    vec![
        Op {
            name: "sessions.ports.publish",
            summary: "Publish a port into a running microVM session, in place (no rebuild, no new microVM): the host's address (`bind`: who may reach it, 127.0.0.1 this machine alone) and `host_port` to its `guest_port`, `protocol` tcp or udp, reaching the members its image gives that port coming in. A microVM session's alone: a session on its machine's host refuses it, naming its engine. Kept on the session and given again whenever its harness starts again; agents_get shows it (state.overrides).",
            read_only: false,
            destructive: false,
            input: Input::of::<input::Publish>(),
            route: |v| route_of(v, &["ports"]),
            run: |cx, v| Box::pin(publish(cx, v)),
        },
        Op {
            name: "sessions.ports.unpublish",
            summary: "Unpublish a port a running microVM session publishes: its `bind`, `host_port` and `protocol`.",
            read_only: false,
            destructive: true,
            input: Input::of::<input::Unpublish>(),
            route: |v| route_of(v, &["ports", "unpublish"]),
            run: |cx, v| Box::pin(unpublish(cx, v)),
        },
        Op {
            name: "sessions.egress.grant",
            summary: "Grant a running microVM session's `member` (`harness`, the default, `agent:<name>` or `mcp:<name>`) egress to `host` (a DNS name or an address, an MCP server's among them) on `port`, in place: nothing else is reached (default deny), and nothing past the most its launch declared it may be granted live (state.overrides.ceiling, its person's or org's policy). A microVM session's alone: a session on its machine's host refuses it, naming its engine.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::Egress>(),
            route: |v| route_of(v, &["egress"]),
            run: |cx, v| Box::pin(egress(cx, v, true)),
        },
        Op {
            name: "sessions.egress.revoke",
            summary: "Revoke egress a running microVM session's `member` was granted: its `host` and `port`; its next connection there is refused.",
            read_only: false,
            destructive: true,
            input: Input::of::<input::Egress>(),
            route: |v| route_of(v, &["egress", "revoke"]),
            run: |cx, v| Box::pin(egress(cx, v, false)),
        },
        Op {
            name: "sessions.networks.connect",
            summary: "Connect a running microVM session's `member` (`harness`, the default, `agent:<name>` or `mcp:<name>`) to the named `network` it shares with the runs connected to it, in place. A microVM session's alone: a session on its machine's host refuses it, naming its engine.",
            read_only: false,
            destructive: false,
            input: Input::of::<input::Network>(),
            route: |v| route_of(v, &["networks"]),
            run: |cx, v| Box::pin(network(cx, v, true)),
        },
        Op {
            name: "sessions.networks.disconnect",
            summary: "Disconnect a running microVM session's `member` from the named `network`.",
            read_only: false,
            destructive: true,
            input: Input::of::<input::Network>(),
            route: |v| route_of(v, &["networks", "disconnect"]),
            run: |cx, v| Box::pin(network(cx, v, false)),
        },
        Op {
            name: "sessions.grants",
            summary: "What a session's launch gave its harness as it started, its base, beside its live overrides (state.overrides, as each sessions.* operation changes them): its `engine`; in `base`, its working `folder`, the machine's folders it sees (`volumes`: each `host` path, where it sees it `at`, `writable`), the folders its harness is allowed (`allowed_dirs`), where its microVM takes a folder live (`plug_roots`), what it reaches past its microVM (`egress`, as `/runs/reach` says it; `{\"as\": \"host\"}` on its host, which narrows nothing) and the most egress it may be granted live (`egress_ceiling`), its credentials by reference (`secrets`: each `name` and the hosts it's filled in `toward`), and `ports` and `networks`, none at a launch (default deny); `reported` is false where its runner is from before it said them. Every engine.",
            read_only: true,
            destructive: false,
            input: Input::of::<input::Grants>(),
            route: |v| Ok((Method::GET, path(&["api", "harness", "sessions", &text(v, "agent")?, "grants"]))),
            run: |cx, v| Box::pin(grants(cx, v)),
        },
        Op {
            name: "sessions.volumes.add",
            summary: "Give a running session a folder of its machine (`path`), `access` ro or rw, in place: it joins its harness's own allowed folders (each harness's way) and the session's scope (kiln holds its file calls there to `access`), and in a microVM session it's plugged into the microVM too, for `members` (else its harness), at `at`: by default beneath its session's volume root (/volumes/<name>); strictly beneath one of the plug roots it declared as it started (state.overrides.roots), else refused. Every engine. Kept on the session and given again whenever its harness starts again; agents_get shows how each took it (state.overrides).",
            read_only: false,
            destructive: false,
            input: Input::of::<input::Add>(),
            route: |v| route_of(v, &["volumes"]),
            run: |cx, v| Box::pin(add(cx, v)),
        },
        Op {
            name: "sessions.volumes.remove",
            summary: "Take a folder (`path`) back from a running session: out of its harness's allowed folders, its scope and, in a microVM, unplugged.",
            read_only: false,
            destructive: true,
            input: Input::of::<input::Remove>(),
            route: |v| route_of(v, &["volumes", "remove"]),
            run: |cx, v| Box::pin(remove(cx, v)),
        },
    ]
}

/// What each operation takes (`crate::ops::Input`).
pub mod input {
    use schemars::JsonSchema;
    use serde::Deserialize;

    pub use super::{Access, Protocol};

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Publish {
        /// The session: an agent's id, as agents.list gives it.
        pub agent: String,
        /// The host address it's published on: who may reach it (127.0.0.1: this machine alone).
        pub bind: String,
        pub host_port: u16,
        /// The member's port in the microVM.
        pub guest_port: u16,
        pub protocol: Protocol,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Unpublish {
        /// The session: an agent's id, as agents.list gives it.
        pub agent: String,
        pub bind: String,
        pub host_port: u16,
        pub protocol: Protocol,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Egress {
        /// The session: an agent's id, as agents.list gives it.
        pub agent: String,
        /// The member granted it: `harness` (the default), `agent:<name>` or `mcp:<name>`.
        pub member: Option<String>,
        /// A DNS name or an address.
        pub host: String,
        pub port: u16,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Network {
        /// The session: an agent's id, as agents.list gives it.
        pub agent: String,
        /// The member: `harness` (the default), `agent:<name>` or `mcp:<name>`.
        pub member: Option<String>,
        /// The network's name.
        pub network: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Grants {
        /// The session: an agent's id, as agents.list gives it.
        pub agent: String,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Add {
        /// The session: an agent's id, as agents.list gives it.
        pub agent: String,
        /// The folder, an absolute path on the session's machine.
        pub path: String,
        pub access: Access,
        /// Where a microVM session's members see it (absolute, in the microVM); by default where kiln plugs that folder
        /// in. A session on its machine's host sees it at its own path.
        pub at: Option<String>,
        /// Who sees it: `harness` (the default), `agent:<name>`, `mcp:<name>`.
        pub members: Option<Vec<String>>,
    }

    #[derive(Debug, Deserialize, JsonSchema)]
    #[serde(deny_unknown_fields)]
    pub struct Remove {
        /// The session: an agent's id, as agents.list gives it.
        pub agent: String,
        /// The folder, as it was added.
        pub path: String,
    }
}

#[cfg(test)]
mod tests;
