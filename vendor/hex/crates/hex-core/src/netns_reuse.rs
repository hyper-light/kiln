//! Which network namespace an `oci` box is given when its owner's earlier box left one (goal 5 extended,
//! step 6; Ada's call of 2026-11-03; docs/research/runtime/goal5-netns-reuse.md). Making a network namespace
//! takes the host's one RTNL and its one `sysctl_lock` for each box, on the create's path or ahead of it
//! (decision 4's pool), which holds concurrency 32 once the cgroup and mount locks are off it
//! (`bench/competitive/probes/create_serial.md`, run 10). A namespace handed from one box of an owner to the
//! owner's next takes the making, the loopback's bringing up and the teardown off the lifecycle. This is the
//! pure core: the idle namespaces by owner, their bounds and what goes past them, who may be given one, and
//! the counts that show each path taken. The node's shell holds each namespace by its descriptor (the core's
//! `T`), checks each as its box goes, before it's kept idle, and closes what goes.
//!
//! A box may share its owner's namespaces only if it couldn't have changed its own: its spec gives capability
//! sets naming neither CAP_NET_ADMIN nor CAP_SYS_ADMIN, its `/proc/sys` is read-only, and its namespace is one
//! the node made, in the node's user namespace. Any other box gets a namespace of its own, never kept
//! ([`Take::Own`]). An owner's idle namespaces go oldest first, so a TCP connection a box left in TIME_WAIT
//! has had the longest to end before its namespace is checked again.

use std::collections::{BTreeMap, VecDeque};

use crate::demand::Ghosts;

/// Why a box was given a namespace of its own or a fresh one, or why an idle namespace wasn't handed on:
/// each counted ([`Counts::refused`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Refused {
    /// The box names no owner: a namespace of its own.
    NoOwner,
    /// Its owner opts out: a namespace of its own.
    OptedOut,
    /// The box could change its namespace: its spec gives no capability sets, so it keeps its creator's,
    /// or names CAP_NET_ADMIN or CAP_SYS_ADMIN in one. Every rtnetlink change, interface ioctl, tun device
    /// and firewall table needs CAP_NET_ADMIN (Linux v6.19 net/core/rtnetlink.c:6885). A namespace of its
    /// own.
    NetAdmin,
    /// Its `/proc/sys` is writable: a box running as the namespace's root writes its writable net sysctls
    /// without CAP_NET_ADMIN (net/sysctl_net.c:42-54, fs/proc/proc_sysctl.c:425-434). A namespace of its own.
    ProcSysWritable,
    /// Its namespace would belong to a user namespace of its own, not the node's, which made the ones kept. A
    /// namespace of its own.
    OwnUserNamespace,
    /// Only other owners' namespaces were idle: a fresh one, theirs kept for them.
    Owner,
    /// A socket is still open in the idle namespace, held by a process outside the box.
    Sockets,
    /// A TCP connection the box closed lingers in TIME_WAIT (60 s, include/net/tcp.h:141).
    TimeWait,
    /// A link other than loopback, or loopback down.
    Links,
    /// An address a fresh namespace doesn't have, or one it does missing.
    Addresses,
    /// A route a fresh namespace doesn't have, or one it does missing.
    Routes,
    /// A policy routing rule a fresh namespace doesn't have, or one it does missing.
    Rules,
    /// An iptables table. An nftables table needs CAP_NET_ADMIN (Linux v6.19 net/netfilter/nfnetlink.c:661),
    /// which no box that may share holds, and its dump would load nf_tables on a host without it
    /// (nfnetlink.c:236), so the check doesn't read it.
    Firewall,
    /// A read the check needs failed or didn't parse, so nothing it would show is known.
    Unreadable,
}

impl Refused {
    /// Every reason, in the order the counts list them.
    pub const ALL: [Self; 14] = [
        Self::NoOwner,
        Self::OptedOut,
        Self::NetAdmin,
        Self::ProcSysWritable,
        Self::OwnUserNamespace,
        Self::Owner,
        Self::Sockets,
        Self::TimeWait,
        Self::Links,
        Self::Addresses,
        Self::Routes,
        Self::Rules,
        Self::Firewall,
        Self::Unreadable,
    ];

    /// The reason as the counts name it.
    pub fn name(self) -> &'static str {
        match self {
            Self::NoOwner => "no owner",
            Self::OptedOut => "owner opted out",
            Self::NetAdmin => "could change its namespace",
            Self::ProcSysWritable => "/proc/sys writable",
            Self::OwnUserNamespace => "a user namespace of its own",
            Self::Owner => "another owner's",
            Self::Sockets => "a socket open",
            Self::TimeWait => "TIME_WAIT",
            Self::Links => "links",
            Self::Addresses => "addresses",
            Self::Routes => "routes",
            Self::Rules => "rules",
            Self::Firewall => "firewall",
            Self::Unreadable => "unreadable",
        }
    }
}

/// What the pool has done. A count stops at `u64::MAX`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Counts {
    /// Idle namespaces handed to their owner's next box, each checked as its box went.
    pub handed_on: u64,
    /// Boxes that may share given a fresh namespace, which the node keeps for their owner.
    pub fresh: u64,
    /// Each refusal, by reason.
    pub refused: BTreeMap<Refused, u64>,
    /// Namespaces closed because their owner kept its bound already.
    pub evicted_owner_bound: u64,
    /// Namespaces closed because the node kept its bound already.
    pub evicted_node_bound: u64,
    /// Namespaces closed because the node let every idle one go.
    pub drained: u64,
    /// Namespaces idle now.
    pub idle: usize,
    /// The most idle namespaces an owner keeps now, learned ([`Idle::new`]).
    pub per_owner: usize,
}

impl Counts {
    /// How many refusals for `reason`.
    pub fn refused(&self, reason: Refused) -> u64 {
        self.refused.get(&reason).copied().unwrap_or(0)
    }
}

/// Who a box is, as a take needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Whose<'a> {
    /// Its owner, if its spec names one.
    pub owner: Option<&'a str>,
    /// Whether its owner lets its boxes share.
    pub reuse: bool,
    /// Whether its spec lets it share, or why not: [`Refused::NetAdmin`], [`Refused::ProcSysWritable`] or
    /// [`Refused::OwnUserNamespace`].
    pub eligible: Result<(), Refused>,
}

/// What a take is answered with.
#[derive(Debug, PartialEq, Eq)]
pub enum Take<T> {
    /// The owner's oldest idle namespace, checked as its box went: [`Idle::hand_on`] counts it handed on.
    Idle(T),
    /// None of the owner's is idle: a fresh namespace, which the node keeps for the owner when the box goes.
    Fresh,
    /// The box may not share: a namespace of its own, never kept, and why.
    Own(Refused),
}

/// The idle namespaces by owner, oldest first in each, and the node's order across owners, each bounded.
#[derive(Debug)]
pub struct Idle<T> {
    /// The most idle namespaces an owner keeps, learned from its misses.
    per_owner: Ghosts,
    per_node: usize,
    by_owner: BTreeMap<String, VecDeque<(u64, T)>>,
    /// When each idle namespace went idle, and whose it is: the oldest first.
    order: BTreeMap<u64, String>,
    seq: u64,
    counts: Counts,
}

impl<T> Idle<T> {
    /// No idle namespaces, bounded at `per_node` for the node and at `per_owner` for one owner at first. The owner
    /// bound is learned as the idle cgroups' is (`reuse::Pool::new`; Ada's rule, 2026-11-06): a fresh take by an
    /// owner whose namespace the bound closed raises it by one, up to `per_node`, never lowered.
    pub fn new(per_owner: usize, per_node: usize) -> Self {
        Self {
            per_owner: Ghosts::new(per_owner, per_node),
            per_node,
            by_owner: BTreeMap::new(),
            order: BTreeMap::new(),
            seq: 0,
            counts: Counts::default(),
        }
    }

    /// A box's namespace: its owner's oldest idle one, a fresh one, or one of its own, counted.
    pub fn take(&mut self, whose: Whose<'_>) -> Take<T> {
        let Some(owner) = whose.owner else {
            self.refuse(Refused::NoOwner);
            return Take::Own(Refused::NoOwner);
        };
        if !whose.reuse {
            self.refuse(Refused::OptedOut);
            return Take::Own(Refused::OptedOut);
        }
        if let Err(why) = whose.eligible {
            self.refuse(why);
            return Take::Own(why);
        }
        if let Some((seq, ns)) = self.by_owner.get_mut(owner).and_then(VecDeque::pop_front) {
            self.order.remove(&seq);
            if self.by_owner.get(owner).is_some_and(VecDeque::is_empty) {
                self.by_owner.remove(owner);
            }
            return Take::Idle(ns);
        }
        if !self.order.is_empty() {
            self.refuse(Refused::Owner);
        }
        self.per_owner.missed(owner);
        self.counts.fresh = self.counts.fresh.saturating_add(1);
        Take::Fresh
    }

    /// The idle namespace last taken was handed on.
    pub fn hand_on(&mut self) {
        self.counts.handed_on = self.counts.handed_on.saturating_add(1);
    }

    /// One refusal for `reason`: a box that may not share, or a namespace given back whose check failed, which
    /// the shell closes.
    pub fn refuse(&mut self, reason: Refused) {
        let n = self.counts.refused.entry(reason).or_insert(0);
        *n = n.saturating_add(1);
    }

    /// `owner`'s box is gone and left `ns`, kept idle for the owner's next box. Returns what the bounds put
    /// out, for the shell to close: the owner's oldest past its bound, then the node's oldest past its.
    pub fn give(&mut self, owner: &str, ns: T) -> Vec<T> {
        self.seq = self.seq.saturating_add(1);
        let seq = self.seq;
        self.order.insert(seq, owner.to_owned());
        let list = self.by_owner.entry(owner.to_owned()).or_default();
        list.push_back((seq, ns));
        let mut out = Vec::new();
        if list.len() > self.per_owner.bound()
            && let Some((old, ns)) = list.pop_front()
        {
            self.order.remove(&old);
            self.per_owner.evicted(owner);
            self.counts.evicted_owner_bound = self.counts.evicted_owner_bound.saturating_add(1);
            out.push(ns);
        }
        // Each pass removes one, so the walk is bounded by how many are idle (CLAUDE.md §11).
        while self.order.len() > self.per_node {
            let Some((_, whose)) = self.order.pop_first() else {
                break;
            };
            if let Some(list) = self.by_owner.get_mut(&whose) {
                if let Some((_, ns)) = list.pop_front() {
                    self.counts.evicted_node_bound =
                        self.counts.evicted_node_bound.saturating_add(1);
                    out.push(ns);
                }
                if list.is_empty() {
                    self.by_owner.remove(&whose);
                }
            }
        }
        out
    }

    /// Every idle namespace, for the shell to close: the node is going, or lets them go.
    #[must_use = "the namespaces handed back are the shell's to close"]
    pub fn drain(&mut self) -> Vec<T> {
        self.order.clear();
        let out: Vec<T> = std::mem::take(&mut self.by_owner)
            .into_values()
            .flat_map(|list| list.into_iter().map(|(_, ns)| ns))
            .collect();
        let n = u64::try_from(out.len()).unwrap_or(u64::MAX);
        self.counts.drained = self.counts.drained.saturating_add(n);
        out
    }

    /// The most idle namespaces an owner keeps now, learned.
    pub fn per_owner(&self) -> usize {
        self.per_owner.bound()
    }

    /// How many of `owner`'s namespaces are idle.
    pub fn idle_of(&self, owner: &str) -> usize {
        self.by_owner.get(owner).map_or(0, VecDeque::len)
    }

    /// What the pool has done.
    pub fn counts(&self) -> Counts {
        Counts {
            idle: self.order.len(),
            per_owner: self.per_owner.bound(),
            ..self.counts.clone()
        }
    }
}

#[cfg(test)]
mod tests;
