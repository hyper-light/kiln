//! A running Shards image box's network and folders, changed in place (kiln plan 01a1dd2e…, P6b.3;
//! docs/design/shards-image-class.md §14; Shards' D269, D271, D278, D279, D285): what a change is ([`Live`]), what a
//! box holds of the changes it took as it runs ([`LiveHeld`]), why one is refused ([`LiveCause`]), and the one check of
//! a change's form ([`check_live`]), which the class and the simulator share, as N5 has one check of a grant's.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::{Class, Grant, Member, Proto, Published, Refusal, check_grants};

/// One change to a running Shards image box's network or folders ([`crate::Runtime::change`]), as its run takes it live.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Live {
    /// A grant of N1 (docs/design/network-grants.md) added as it runs: a `Publish` bound on the host (Shards' D269) and
    /// held to its member alone (N6); an `Egress` or `Host` grant added to its member's reach, which Shards is given
    /// whole (D278). One it holds already is done already.
    Grant(Grant),
    /// That grant taken back: a `Publish` let go, an `Egress` or `Host` grant out of its member's reach.
    Revoke(Grant),
    /// `member` on `network`, a network shared with other runs, under `grant` (Shards' D285): on it from its first
    /// grant there. One it holds already is done already.
    Connect {
        member: Member,
        network: String,
        grant: NetworkGrant,
    },
    /// That grant of `member`'s on `network` taken back: the flows it let open reset, and the member off the network
    /// with its last grant there.
    Disconnect {
        member: Member,
        network: String,
        grant: NetworkGrant,
    },
    /// A host directory plugged into its members (Shards' D271, with D279's `exec`).
    Plug(ImagePlug),
    /// The directory plugged in at `at` unplugged.
    Unplug { at: String },
}

impl Live {
    /// Its name, as a refusal of it says it.
    pub fn op(&self) -> &'static str {
        match self {
            Self::Grant(_) => "grant",
            Self::Revoke(_) => "revoke",
            Self::Connect { .. } => "connect",
            Self::Disconnect { .. } => "disconnect",
            Self::Plug(_) => "plug",
            Self::Unplug { .. } => "unplug",
        }
    }

    /// Whether its run's guest must answer it: a member's reach, a connection and a plug are its guest's as well as its
    /// network process's or its host's, so none is made while its guest is paused; a publish and its letting go are its
    /// host's network process's alone, made while it is paused too (Shards' D269).
    pub fn needs_guest(&self) -> bool {
        !matches!(
            self,
            Self::Grant(Grant::Publish { .. }) | Self::Revoke(Grant::Publish { .. })
        )
    }
}

/// Which way a [`NetworkGrant`] lets a flow open on a network shared by runs (Shards' D285): from its peer to its member
/// (`In`), or from its member to its peer (`Out`). A flow's replies come back by its state, whichever way it opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Way {
    In,
    Out,
}

/// A member's grant on a network shared by runs (Shards' D285), every part named (default deny): its name on the
/// network, as its peers name it, one member's there; its peer, a member of another run on it, by its name there, which
/// outlasts that run's restarts; one protocol's ports, `first` to `last` (its peer's where it opens `Out`, its member's
/// where it opens `In`); and which way a flow opens. Names compare lowercased (RFC 4343), as a network holds them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkGrant {
    pub name: String,
    pub peer: String,
    pub proto: Proto,
    pub first: u16,
    pub last: u16,
    pub way: Way,
}

impl NetworkGrant {
    /// Longest name on a network: a host's name in text form (RFC 1035 §2.3.4), as Shards' `shared::HOST_MOST` bounds
    /// one.
    pub const MAX_NAME: usize = 253;

    /// Why it is no grant, as Shards' `NetworkGrant::refused` says (its run host refuses it again): a name or peer of no
    /// name's grammar, Docker's `[a-z0-9][a-z0-9_.-]*` lowercased (moby's `RestrictedNamePattern`, Shards'
    /// `shared::name_ok`) within [`NetworkGrant::MAX_NAME`]; its peer its own name; port 0, which RFC 6335 §6
    /// reserves; or a first port past its last.
    fn check(&self, field: &str) -> Result<(), String> {
        for (what, name) in [("name", &self.name), ("peer", &self.peer)] {
            if !network_name(name) {
                return Err(format!(
                    "{field}.{what} {name:?} is no name on a network: a letter or digit, then letters, digits, '_', \
                     '.' and '-', at most {} bytes",
                    Self::MAX_NAME
                ));
            }
        }
        if self.name.eq_ignore_ascii_case(&self.peer) {
            return Err(format!("{field}: {:?} is its own peer", self.name));
        }
        if self.first == 0 || self.first > self.last {
            return Err(format!(
                "{field}: ports {} to {}: a range from 1, its first not past its last",
                self.first, self.last
            ));
        }
        Ok(())
    }
}

/// Whether `name` is a name on a network shared by runs: Docker's grammar, lowercased (RFC 4343), within
/// [`NetworkGrant::MAX_NAME`] (Shards' `shared::name_ok`). A network's own name is spelled the same way.
fn network_name(name: &str) -> bool {
    let lowered = name.to_ascii_lowercase();
    let mut bytes = lowered.bytes();
    let rest =
        |b: u8| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'_' | b'.' | b'-');
    lowered.len() <= NetworkGrant::MAX_NAME
        && bytes
            .next()
            .is_some_and(|first| first.is_ascii_lowercase() || first.is_ascii_digit())
        && bytes.all(rest)
}

/// A host directory plugged into a running box's members as it runs (Shards' D271 `Plug`): `source`, absolute on the
/// host its node runs on, at `at` in its members' view, absolute and plain, strictly beneath one of its run's plug roots
/// ([`crate::ShardsImage::plug_roots`]) or its shares' own top-level directories; for `members` (none: every member that
/// runs), read-only where it says, and its files run from it only where `exec` (else each member's mount of it is
/// `noexec`, D279). Left out of JSON where on, `exec` reads as on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImagePlug {
    pub source: PathBuf,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub members: Vec<Member>,
    pub at: String,
    pub read_only: bool,
    #[serde(default = "on", skip_serializing_if = "is_on")]
    pub exec: bool,
}

fn on() -> bool {
    true
}

fn is_on(exec: &bool) -> bool {
    *exec
}

/// A member's connection to a network shared by runs, as a running box holds it ([`LiveHeld::networks`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    pub member: Member,
    pub network: String,
    pub grant: NetworkGrant,
}

/// What a running Shards image box took as it runs, beside its spec ([`crate::BoxInfo::live`]; §14), each held now, in
/// the order it was taken: its grants (none of its spec's among them), its live publishes' ports as its host bound
/// them (each naming its grant by its place in `grants`), its members' connections to shared networks, and its plugs.
/// Its run's: none once its run has ended, and none in its record, which its spec alone makes.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct LiveHeld {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub grants: Vec<Grant>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub published: Vec<Published>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub networks: Vec<Connection>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plugs: Vec<ImagePlug>,
}

impl LiveHeld {
    /// Whether it holds nothing.
    pub fn is_empty(&self) -> bool {
        self.grants.is_empty()
            && self.published.is_empty()
            && self.networks.is_empty()
            && self.plugs.is_empty()
    }
}

/// Why a running box's change was refused ([`Refusal::Live`]): where the box stands, then Shards' own causes, a network
/// change's ([`NetCause`]) or a plug's ([`PlugCause`]). The node's wire tags each by its place here, so append only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum LiveCause {
    /// It has no run: it hasn't started, or its stop has begun.
    #[error("it has no run: it hasn't started, or its stop has begun")]
    NotRunning,
    /// Its run has ended.
    #[error("its run has ended")]
    Ended,
    /// It is of a class or form whose boxes take no change as they run: every one but a Shards image box.
    #[error("only a Shards image box takes changes as it runs, and it is of class {}", .0.name())]
    Class(Class),
    /// Its guest joins no network shared by runs (Shards' `ImageRefusal::Unsupported`).
    #[error("its guest takes no such change")]
    Unsupported,
    /// Its network process or its guest refused a network change.
    #[error("{0}")]
    Net(NetCause),
    /// Its run host refused a plug or an unplug.
    #[error("{0}")]
    Plug(PlugCause),
}

/// Why a running box's network change was refused, Shards' causes one for one (`shards_api::image::NetRefusal`, D269,
/// D278, D285), in their order there. The node's wire tags each by its place here, so append only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum NetCause {
    /// Its run has no network device: it started with nothing published, kept or linked past its microVM.
    #[error("its run has no network device")]
    NoNetwork,
    /// Past what its image grants and its ceiling ([`crate::ShardsImage::ceiling`]) let it be given.
    #[error("past its image's grants and its ceiling")]
    Widening,
    /// The host's port could not be bound: taken, or refused by the host.
    #[error("its host couldn't bind the port")]
    Bind,
    /// Nothing of it is held to take back, or a member that runs not.
    #[error("nothing of it is held there")]
    Unknown,
    /// Its network process did not take it, or has ended.
    #[error("its network process didn't take it")]
    Network,
    /// Its image's own declaration of it: a port it gives a member that is none of its run's.
    #[error("its image gives that port to no member of its run")]
    Image,
    /// Members whose reach, so changed, its microVM cannot hold apart.
    #[error("its microVM couldn't hold its members apart so")]
    Unheld,
    /// No network of that name in its run's home: one is made beforehand, never by a connection.
    #[error("no such network")]
    NoSuchNetwork,
    /// The grant's name is another member's on that network.
    #[error("that name is another member's on the network")]
    NameTaken,
    /// Every address of the network's range is another run's.
    #[error("the network has no address left")]
    Exhausted,
    /// Another process links that network's runs.
    #[error("another process links that network's runs")]
    PairedElsewhere,
    /// The network has IPv6, which a network shared by runs does not carry yet.
    #[error("the network has IPv6")]
    Ipv6,
    /// A grant that is none, as its run host reads it.
    #[error("its grant is none")]
    Grant,
    /// The network's subnet meets another of its microVM's networks.
    #[error("the network's subnet meets another of its microVM's")]
    Overlaps,
}

/// Why a running box's plug or unplug was refused, Shards' causes one for one (`shards_api::image::PlugRefusal`, D271,
/// D281), in their order there. The node's wire tags each by its place here, so append only.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum PlugCause {
    /// The ask itself: a path that is not absolute, a member its image does not declare.
    #[error("the plug isn't one")]
    Ask,
    /// A member it names runs nothing: files alone, or ended.
    #[error("a member it names runs nothing")]
    Member,
    /// Its place lies beneath none of its run's plug roots.
    #[error("its place is beneath none of its plug roots")]
    Root,
    /// Something is plugged in at its place already.
    #[error("something is plugged in there already")]
    Taken,
    /// The host's directory could not be opened, or served.
    #[error("its host directory couldn't be served")]
    Source,
    /// Its microVM could not take it: no slot free, or its guest refused it.
    #[error("its microVM couldn't take it")]
    Device,
    /// An unplug's: nothing is plugged in there.
    #[error("nothing is plugged in there")]
    Unplugged,
    /// An unplug's: something in a member uses it.
    #[error("a member uses it")]
    InUse,
    /// A volume its home's store would not give.
    #[error("its home's store gives no such volume")]
    Volume,
    /// A volume asked with another bound than its holders serve it with.
    #[error("its volume is held with another bound")]
    Bound,
    /// A volume a clearing holds alone.
    #[error("its volume is being cleared")]
    Clearing,
}

/// Whether `change` is well formed for a box whose spec grants `granted` and which holds `held` as it runs, refused
/// `Invalid` by the field that fails, never dropped or half-applied (CLAUDE.md §3): a grant by N5's one check, with the
/// grants the box holds, so that no flow is named twice (one it holds already passes, a change done already); a
/// connection's member, network and grant by Shards' rules; a plug's source absolute, its place absolute and plain, and
/// each member named once. Whether it is held, or fits its run's roots and ceiling, is its run's to answer.
pub fn check_live(change: &Live, granted: &[Grant], held: &LiveHeld) -> Result<(), Refusal> {
    let invalid = |why: String| Refusal::Invalid {
        what: "change",
        why,
    };
    match change {
        Live::Grant(grant) => {
            if granted.contains(grant) || held.grants.contains(grant) {
                return check_grants(std::slice::from_ref(grant));
            }
            let mut all: Vec<Grant> = Vec::new();
            let count = granted
                .len()
                .checked_add(held.grants.len())
                .and_then(|n| n.checked_add(1))
                .ok_or_else(|| invalid("its box's grants pass a usize".to_owned()))?;
            all.try_reserve_exact(count)
                .map_err(|_| Refusal::Exhausted {
                    what: "grants",
                    bound: count,
                })?;
            all.extend(granted.iter().cloned());
            all.extend(held.grants.iter().cloned());
            all.push(grant.clone());
            check_grants(&all)
        }
        Live::Revoke(grant) => check_grants(std::slice::from_ref(grant)),
        Live::Connect {
            member,
            network,
            grant,
        }
        | Live::Disconnect {
            member,
            network,
            grant,
        } => {
            member.check_named("change.member").map_err(invalid)?;
            if !network_name(network) {
                return Err(invalid(format!(
                    "change.network {network:?} is no network's name: a letter or digit, then letters, digits, '_', \
                     '.' and '-', at most {} bytes",
                    NetworkGrant::MAX_NAME
                )));
            }
            grant.check("change.grant").map_err(invalid)
        }
        Live::Plug(plug) => {
            // Its host directory as a share's is held (its spec's `host_path`): UTF-8 with no NUL, and absolute in either
            // form its node's host may give, rooted (`/…`) or drive-absolute (`C:\…`).
            let Some(source) = plug.source.to_str() else {
                return Err(invalid(format!(
                    "change.source {:?} isn't UTF-8",
                    plug.source
                )));
            };
            if source.contains('\0') || (!source.starts_with('/') && !crate::drive_absolute(source))
            {
                return Err(invalid(format!(
                    "change.source {source:?} isn't an absolute path on its host"
                )));
            }
            plain(&plug.at, "change.at").map_err(invalid)?;
            for (at, member) in plug.members.iter().enumerate() {
                member
                    .check_named(&format!("change.members[{at}]"))
                    .map_err(invalid)?;
                if plug
                    .members
                    .iter()
                    .take(at)
                    .any(|earlier| earlier == member)
                {
                    return Err(invalid(format!("change.members names {member} twice")));
                }
            }
            Ok(())
        }
        Live::Unplug { at } => plain(at, "change.at").map_err(invalid),
    }
}

/// Whether `path` is a place in a member's view a plug goes: absolute, with no empty, `.` or `..` component, no NUL, and
/// below its top level (a top-level directory is a root's, never a plug's: Shards' D271 plugs strictly beneath one).
fn plain(path: &str, field: &str) -> Result<(), String> {
    let Some(rest) = path.strip_prefix('/') else {
        return Err(format!("{field} {path:?} isn't absolute"));
    };
    if path.contains('\0') {
        return Err(format!("{field} {path:?} holds a NUL"));
    }
    if rest
        .split('/')
        .any(|step| step.is_empty() || step == "." || step == "..")
    {
        return Err(format!(
            "{field} {path:?} has an empty, '.' or '..' component"
        ));
    }
    if rest.split('/').nth(1).is_none() {
        return Err(format!(
            "{field} {path:?} is a top-level directory: a plug goes strictly beneath one of its roots"
        ));
    }
    Ok(())
}

/// Whether `root` is a plug root (Shards' `plug::root_name`, D235): one component past `/`, not `.` or `..`, with no
/// NUL, and none of the directories shards-init mounts over in a member's view (`proc`, `sys`, `dev`, `tmp`, `etc`,
/// `run`: Shards' `plug::REFUSED`).
pub fn check_plug_root(root: &str, field: &str) -> Result<(), Refusal> {
    const REFUSED: [&str; 6] = ["proc", "sys", "dev", "tmp", "etc", "run"];
    let ok = root.strip_prefix('/').is_some_and(|name| {
        !name.is_empty()
            && !name.contains('/')
            && !name.contains('\0')
            && name != "."
            && name != ".."
            && !REFUSED.contains(&name)
    });
    if ok {
        Ok(())
    } else {
        Err(Refusal::Invalid {
            what: "plug root",
            why: format!(
                "{field} {root:?} is no plug root: one top-level directory of its members' view, none of /proc, \
                 /sys, /dev, /tmp, /etc and /run"
            ),
        })
    }
}
