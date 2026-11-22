//! The host transports authenticated messages; Amphora owns replication and membership.
use serde::{Deserialize, Serialize};

/// An explicit original database identity, shared by every initial group member.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bootstrap {
    pub source_node: u64,
    pub source_id: [u8; 32],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaConfig {
    pub node_id: u64,
    pub voters: Vec<u64>,
    pub learners: Vec<u64>,
    #[serde(default)]
    pub bootstrap: Option<Bootstrap>,
}
/// One change of a replicated group's members (replication.md, "Membership"). The group's leader
/// makes it through a joint configuration and answers once the group has left that configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MemberChange {
    /// A new member. It is sent every entry and votes in nothing.
    AddLearner(u64),
    /// A learner made a voter.
    Promote(u64),
    /// A voter made a learner.
    Demote(u64),
    /// A member removed. Nothing more is sent to it.
    Remove(u64),
}

/// When a replicated database acknowledges a write (docs/design/replication.md, "Fast acknowledgement:
/// the replicated class").
///
/// A frozen format: its postcard encoding is written into a group's log (the class entry) and into
/// each member's database (`__amphora_class`; replication.md, "The class stated once per group").
/// Variants and fields are only ever appended, and none is reordered or retyped.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Durability {
    /// Once a quorum's disks hold it and the proposer's own log is flushed. Each member flushes its
    /// database's log behind its owner, and a restart applies again from its own raft log what that
    /// flush hadn't covered (docs/design/replication.md, "A replica's database log").
    #[default]
    Persisted,
    /// Once members spanning `spanning` distinct fates hold it, in memory or on disk. Losing every
    /// member of one fate loses nothing acknowledged; losing every fate at once can lose what was
    /// acknowledged past the members' last durable promises.
    Replicated(Fates),
}

/// What each member fails with, and how many distinct fates a write's holders must span.
///
/// A frozen format, as [`Durability`] is: its postcard encoding is in a group's log and its members'
/// databases, so its fields are only ever appended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Fates {
    /// Each member and its fate: the operator's number for what it fails with, its host and its
    /// power feed. Every member states one.
    pub of: Vec<(u64, u64)>,
    /// The distinct fates a write's holders must span before it is acknowledged: at least 2.
    pub spanning: u32,
}

/// What the host's node-level failure detector (hyper-liveness) concluded of a member's node,
/// told to each replicated database the node shares with it (docs/design/replication.md, "The
/// suspicion feed"). Under the replicated class a suspected member's held answers aren't counted
/// until it is trusted again; elections never take a verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// `Change::Suspected`, or `Change::Stopped`: the member's node is suspected.
    Suspected(u64),
    /// `Change::Trusted`, or `Change::Restarted`: the member's node is trusted again.
    Trusted(u64),
}

/// Where catching up a learner stands, as its group's leader judges it (replication.md,
/// "Membership"; Ongaro's thesis section 4.2.1): replication to it goes in rounds, each sending
/// what the leader held as the round began. A learner promoted once ready doesn't stall commits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CatchUp {
    /// A round ended within an election timeout: it may vote without leaving a gap.
    Ready,
    /// Its rounds are under way.
    Pending,
    /// Its lag didn't shrink over a whole election timeout: it is unavailable, or slower than the
    /// leader appends. Asked again, it is judged afresh.
    Aborted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outbound {
    pub to: u64,
    pub bytes: Vec<u8>,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicaStatus {
    pub node_id: u64,
    pub leader: u64,
    pub term: u64,
    pub applied: u64,
    pub durable_commit: u64,
    pub voters: Vec<u64>,
    pub learners: Vec<u64>,
    pub fenced: Option<String>,
    /// Initial data is not exposed until its declared image is committed and applied.
    #[serde(default)]
    pub bootstrapping: bool,
    /// The voters a joint configuration leaves (replication.md, "Membership"): empty unless the
    /// group is between two configurations.
    #[serde(default)]
    pub voters_outgoing: Vec<u64>,
}

/// The application floor a replica applied, and where each member of its configuration stands
/// against it (replication.md, "The seam": `floor_status`). Read from what the owner published at
/// its last applied index, without asking it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FloorStatus {
    /// The floor's application, none before a floor is raised.
    pub application: Option<String>,
    /// The floor, 0 for none.
    pub version: u32,
    /// Every member of the configuration in force, by identity.
    pub members: Vec<MemberFloor>,
}

/// One member against the floor: whether it votes, and the highest version of the floor's
/// application it attested under its current join, if it has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemberFloor {
    pub member: u64,
    pub voter: bool,
    pub attested: Option<u32>,
}
