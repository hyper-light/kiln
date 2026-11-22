//! Region-local groups under a global root (CP-T1.11, G14 step 1; docs/design/regions.md): a pure
//! core, no clock, disk or network. Each member carries a region label ([`Members`]); a group's
//! [`Rule`] says where its voters and its lead may be, CockroachDB's placement policies made
//! Amphora's: a regional group ([`Rule::Home`]) has every voter and its lead in its home region,
//! so it commits at that region's latency; the global root ([`Rule::Global`]) spreads its voters
//! over regions so that losing the regions it is declared to survive leaves it quorate.
//!
//! [`place`] picks voters from the live members by the rule, keeping every prior voter that still
//! fits (a lost member is replaced, nothing else moves), and refuses with a typed [`Refused`] when
//! the rule can't be met: a regional group never borrows a voter from another region.
//!
//! [`Atlas`] routes: regional rows (CockroachDB's `REGIONAL BY ROW`) live in their home region's
//! directory, global rows in the root's. A write goes to its home's group; a read that doesn't
//! name a home tries the reader's own region first, then the others in region order
//! (CockroachDB's locality-optimized search). Each directory is a [`Directory`] like any other,
//! so each region reshards on its own by CP-T1.9's operations.
//!
//! [`lab`] runs a record corpus through this routing on a simulated topology whose latencies are
//! its input, at one node, three nodes or three regions, with the same results.

pub mod lab;

use std::collections::{BTreeMap, BTreeSet};

use crate::reshard::directory::Directory;
use crate::reshard::{GroupId, NodeId, position};

/// A region's label, as the operator names it when a member joins.
#[derive(
    Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, serde::Serialize, serde::Deserialize,
)]
pub struct Region(pub String);

/// What placement refuses, and why. Nothing is placed by a refused request.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refused {
    /// A home region with fewer live members than its rule's voters.
    #[error("region {region:?} has {live} live members, {needs} needed")]
    TooFewMembers {
        region: Region,
        live: usize,
        needs: usize,
    },
    /// Fewer regions able to hold the root's voters than it is declared to survive the loss of.
    #[error("{have} regions can hold the root's voters, {needs} needed")]
    TooFewRegions { have: usize, needs: usize },
    /// A member no region label was given for.
    #[error("member {0:?} has no region")]
    Unknown(NodeId),
    /// A placement that breaks its rule.
    #[error("placement breaks its rule: {0}")]
    Breaks(&'static str),
    /// A read or write for a region the database has no directory in.
    #[error("no directory in region {0:?}")]
    NoRegion(Region),
    /// A directory places no group at the key's position (a directory broken in memory).
    #[error("no group holds the key")]
    NoGroup,
    /// A count past what fits in memory's arithmetic.
    #[error("{0} overflows")]
    Overflow(&'static str),
}

/// Every member with its region, and which are down now.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Members {
    regions: BTreeMap<NodeId, Region>,
    down: BTreeSet<NodeId>,
}

impl Members {
    /// No members.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// `node` joins, labelled `region`; a member joining again takes its new label.
    pub fn join(&mut self, node: NodeId, region: Region) {
        self.regions.insert(node, region);
        self.down.remove(&node);
    }

    /// `node` is lost (down or partitioned away); its label stays.
    pub fn lose(&mut self, node: NodeId) {
        if self.regions.contains_key(&node) {
            self.down.insert(node);
        }
    }

    /// `node` is back.
    pub fn restore(&mut self, node: NodeId) {
        self.down.remove(&node);
    }

    /// Every member of `region` is lost: the region-loss drill's cut.
    pub fn lose_region(&mut self, region: &Region) {
        let lost: Vec<NodeId> = self.in_region(region).collect();
        self.down.extend(lost);
    }

    /// `node`'s region.
    #[must_use]
    pub fn region_of(&self, node: NodeId) -> Option<&Region> {
        self.regions.get(&node)
    }

    /// Whether `node` is a member and not lost.
    #[must_use]
    pub fn live(&self, node: NodeId) -> bool {
        self.regions.contains_key(&node) && !self.down.contains(&node)
    }

    /// Every region a member is labelled with, in order.
    #[must_use]
    pub fn regions(&self) -> BTreeSet<Region> {
        self.regions.values().cloned().collect()
    }

    fn in_region<'a>(&'a self, region: &'a Region) -> impl Iterator<Item = NodeId> + 'a {
        self.regions
            .iter()
            .filter(move |(_, r)| *r == region)
            .map(|(n, _)| *n)
    }

    /// The live members of `region`, in node order.
    #[must_use]
    pub fn live_in(&self, region: &Region) -> Vec<NodeId> {
        self.in_region(region).filter(|n| self.live(*n)).collect()
    }
}

/// Where a group's voters and lead may be: CockroachDB's survival goals, as a count of failures
/// declared, from which the voters follow (a quorum of `2f + 1` survives `f` lost).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Rule {
    /// A regional group: `2 * survive + 1` voters and the lead, all in `region` (CockroachDB's
    /// `SURVIVE ZONE FAILURE` with the voters constrained to the home region).
    Home { region: Region, survive: usize },
    /// The global root: `2 * survive + 1` voters in each region that can hold them, over at least
    /// `2 * survive_regions + 1` regions, so losing any `survive_regions` of them leaves a quorum
    /// (CockroachDB's `SURVIVE REGION FAILURE`); the lead in `lead`, if named (its primary
    /// region).
    Global {
        survive_regions: usize,
        survive: usize,
        lead: Option<Region>,
    },
}

/// A group's voters and its lead (one of them).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Placement {
    /// In node order.
    pub voters: Vec<NodeId>,
    pub lead: NodeId,
}

/// `2 * f + 1`: the voters a quorum that survives `f` lost needs.
fn quorum_of(f: usize) -> Result<usize, Refused> {
    f.checked_mul(2)
        .and_then(|v| v.checked_add(1))
        .ok_or(Refused::Overflow("voters"))
}

/// `needs` voters from `live`, keeping each of `prior` that is in it first (so a lost member is
/// replaced and nothing else moves), then the rest in node order.
fn pick(live: &[NodeId], prior: &[NodeId], needs: usize) -> Vec<NodeId> {
    let kept = live.iter().filter(|n| prior.contains(n));
    let fresh = live.iter().filter(|n| !prior.contains(n));
    let mut voters: Vec<NodeId> = kept.chain(fresh).take(needs).copied().collect();
    voters.sort_unstable();
    voters
}

/// The lead among `voters`: the prior lead if it is still one of them and fits `within`, else
/// the first voter in `within` (any voter when `within` is none).
fn lead_of(
    members: &Members,
    voters: &[NodeId],
    prior: Option<NodeId>,
    within: Option<&Region>,
) -> Result<NodeId, Refused> {
    let fits = |n: &NodeId| within.is_none_or(|r| members.region_of(*n) == Some(r));
    if let Some(p) = prior.filter(|p| voters.contains(p) && fits(p)) {
        return Ok(p);
    }
    voters
        .iter()
        .copied()
        .find(|n| fits(n))
        .ok_or(Refused::Breaks("no voter in the lead's region"))
}

/// A placement for `rule` on `members`' live ones, as close to `prior` as the rule allows.
pub fn place(
    members: &Members,
    rule: &Rule,
    prior: Option<&Placement>,
) -> Result<Placement, Refused> {
    let prior_voters = prior.map_or(&[][..], |p| p.voters.as_slice());
    let prior_lead = prior.map(|p| p.lead);
    match rule {
        Rule::Home { region, survive } => {
            let needs = quorum_of(*survive)?;
            let live = members.live_in(region);
            if live.len() < needs {
                return Err(Refused::TooFewMembers {
                    region: region.clone(),
                    live: live.len(),
                    needs,
                });
            }
            let voters = pick(&live, prior_voters, needs);
            let lead = lead_of(members, &voters, prior_lead, Some(region))?;
            Ok(Placement { voters, lead })
        }
        Rule::Global {
            survive_regions,
            survive,
            lead,
        } => {
            let each = quorum_of(*survive)?;
            let needs = quorum_of(*survive_regions)?;
            let mut voters = Vec::new();
            let mut able = 0usize;
            for region in members.regions() {
                let live = members.live_in(&region);
                if live.len() >= each {
                    voters.extend(pick(&live, prior_voters, each));
                    able = able.checked_add(1).ok_or(Refused::Overflow("regions"))?;
                }
            }
            if able < needs {
                return Err(Refused::TooFewRegions { have: able, needs });
            }
            voters.sort_unstable();
            let lead = lead_of(members, &voters, prior_lead, lead.as_ref())?;
            Ok(Placement { voters, lead })
        }
    }
}

/// Whether `placement` keeps `rule` on `members` (counting every voter, live or not: a placement
/// is checked as placed, and [`quorate`] says whether it can commit now).
pub fn check(members: &Members, rule: &Rule, placement: &Placement) -> Result<(), Refused> {
    let mut seen = BTreeSet::new();
    let mut by_region: BTreeMap<&Region, usize> = BTreeMap::new();
    for v in &placement.voters {
        if !seen.insert(*v) {
            return Err(Refused::Breaks("a voter twice"));
        }
        let region = members.region_of(*v).ok_or(Refused::Unknown(*v))?;
        let n = by_region.entry(region).or_default();
        *n = n.checked_add(1).ok_or(Refused::Overflow("voters"))?;
    }
    if !seen.contains(&placement.lead) {
        return Err(Refused::Breaks("the lead is not a voter"));
    }
    let lead_region = members
        .region_of(placement.lead)
        .ok_or(Refused::Unknown(placement.lead))?;
    match rule {
        Rule::Home { region, survive } => {
            if by_region.keys().any(|r| *r != region) || lead_region != region {
                return Err(Refused::Breaks(
                    "a voter or the lead outside the home region",
                ));
            }
            if placement.voters.len() != quorum_of(*survive)? {
                return Err(Refused::Breaks("not the rule's count of voters"));
            }
        }
        Rule::Global {
            survive_regions,
            survive,
            lead,
        } => {
            let each = quorum_of(*survive)?;
            if by_region.values().any(|n| *n != each) {
                return Err(Refused::Breaks("a region without the rule's voters"));
            }
            if by_region.len() < quorum_of(*survive_regions)? {
                return Err(Refused::Breaks("too few regions"));
            }
            if lead.as_ref().is_some_and(|l| l != lead_region) {
                return Err(Refused::Breaks("the lead outside its region"));
            }
            // Losing the `survive_regions` regions holding the most voters leaves a majority.
            let mut counts: Vec<usize> = by_region.values().copied().collect();
            counts.sort_unstable_by(|a, b| b.cmp(a));
            let lost: usize = counts.iter().take(*survive_regions).sum();
            let left = placement.voters.len().saturating_sub(lost);
            if left.checked_mul(2).ok_or(Refused::Overflow("voters"))? <= placement.voters.len() {
                return Err(Refused::Breaks(
                    "losing a region it survives leaves no quorum",
                ));
            }
        }
    }
    Ok(())
}

/// Whether a majority of `placement`'s voters is live: whether it can commit now.
#[must_use]
pub fn quorate(members: &Members, placement: &Placement) -> bool {
    let live = placement
        .voters
        .iter()
        .filter(|v| members.live(**v))
        .count();
    live.checked_mul(2)
        .is_some_and(|l| l > placement.voters.len())
}

/// Which directory a group is in: the root's (global rows) or a region's.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Scope {
    Global,
    Region(Region),
}

/// A group, named by its directory and its id there.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Address {
    pub scope: Scope,
    pub group: GroupId,
}

/// The root's directory (global rows; the root also keeps every region's directory as records,
/// replicated by its [`Rule::Global`] placement) and each region's directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Atlas {
    root: Directory,
    regions: BTreeMap<Region, Directory>,
}

impl Atlas {
    /// The root's directory and one for each region.
    #[must_use]
    pub fn new(root: Directory, regions: BTreeMap<Region, Directory>) -> Self {
        Self { root, regions }
    }

    /// The directory of `scope`.
    pub fn directory(&self, scope: &Scope) -> Result<&Directory, Refused> {
        match scope {
            Scope::Global => Ok(&self.root),
            Scope::Region(r) => self
                .regions
                .get(r)
                .ok_or_else(|| Refused::NoRegion(r.clone())),
        }
    }

    /// Every group, with its scope.
    #[must_use]
    pub fn groups(&self) -> Vec<Address> {
        let root = self.root.groups().map(|(group, _)| Address {
            scope: Scope::Global,
            group,
        });
        let regional = self.regions.iter().flat_map(|(r, d)| {
            d.groups().map(move |(group, _)| Address {
                scope: Scope::Region(r.clone()),
                group,
            })
        });
        root.chain(regional).collect()
    }

    /// The group a write of `key` homed in `scope` goes to.
    pub fn write(&self, scope: &Scope, key: &str) -> Result<Address, Refused> {
        let group = self
            .directory(scope)?
            .owner(position(key))
            .ok_or(Refused::NoGroup)?;
        Ok(Address {
            scope: scope.clone(),
            group,
        })
    }

    /// The groups a read of regional `key` from `from` tries, in order: its own region's first,
    /// then each other region's, in region order.
    pub fn read(&self, from: &Region, key: &str) -> Result<Vec<Address>, Refused> {
        let local = self
            .regions
            .get_key_value(from)
            .ok_or_else(|| Refused::NoRegion(from.clone()))?;
        let others = self.regions.iter().filter(|(r, _)| *r != from);
        std::iter::once(local)
            .chain(others)
            .map(|(r, d)| {
                let group = d.owner(position(key)).ok_or(Refused::NoGroup)?;
                Ok(Address {
                    scope: Scope::Region(r.clone()),
                    group,
                })
            })
            .collect()
    }
}
