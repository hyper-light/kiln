//! The simulated region lab (CP-T1.11; AC-T1.11.2, AC-T1.11.4): a record corpus routed by
//! [`Atlas`] over groups placed by [`place`], on a topology whose round-trip times are the lab's
//! input ([`Latency`]): nothing here picks a latency. Deterministic: no clock, no randomness.
//!
//! The database's regions (the labels rows are homed in) are the same at every scale; the
//! members' regions are the topology's. A database region with members of its own label is
//! placed there; one without (every region but the primary, on one node or one region of three
//! nodes) is placed in the primary region, the first the lab is given: the one choice a smaller
//! topology can't avoid (CLAUDE.md §8). So the corpus's answers and rows are the same at one node,
//! three nodes and three regions, and only where they commit, and how fast, differs.
//!
//! A write commits when its group's lead has a quorum: the client's round trip to the lead, then
//! the lead's round trip to the voter that completes its majority (itself counting as one). A
//! read is answered by the group's lead (its lease), trying the reader's region first and, on a
//! miss, the other regions at once.

use std::collections::BTreeMap;
use std::time::Duration;

use super::{Address, Atlas, Members, Placement, Refused, Region, Rule, Scope, place, quorate};
use crate::reshard::NodeId;

/// Round-trip times: within a region, and between any two regions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Latency {
    pub within: Duration,
    pub between: Duration,
}

impl Latency {
    fn rtt(&self, a: &Region, b: &Region) -> Duration {
        if a == b { self.within } else { self.between }
    }
}

/// A topology: each region the members are in (the first is the primary) with its member
/// count, the database's regions, each scope's group count, and the failures each rule survives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shape {
    pub members: Vec<(Region, usize)>,
    pub homes: Vec<Region>,
    pub groups: usize,
    pub survive: usize,
    pub survive_regions: usize,
}

/// One operation of the corpus, by a client homed in `homes[client % homes.len()]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    /// Write `key` = `value`: a global row (in the root's directory), or a regional row homed in
    /// the client's region.
    Put {
        client: usize,
        key: String,
        value: u64,
        global: bool,
    },
    /// Read `key`: the global row, or the regional row the client's locality-optimized search
    /// finds first.
    Get {
        client: usize,
        key: String,
        global: bool,
    },
}

/// What the corpus did: its answers and rows (the same at every scale), and how long each
/// regional and global write took to commit (the topology's).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    pub answers: Vec<Option<u64>>,
    pub rows: BTreeMap<(Scope, String), u64>,
    pub regional_commits: Vec<Duration>,
    pub global_commits: Vec<Duration>,
}

/// The lab: members, routing, each group's rule and placement, and each group's rows.
#[derive(Debug, Clone)]
pub struct Lab {
    members: Members,
    latency: Latency,
    homes: Vec<Region>,
    atlas: Atlas,
    rules: BTreeMap<Address, Rule>,
    placements: BTreeMap<Address, Placement>,
    rows: BTreeMap<Address, BTreeMap<String, u64>>,
}

impl Lab {
    /// Members numbered from 0 in `shape`'s order, every group placed by its rule.
    pub fn new(shape: &Shape, latency: Latency) -> Result<Self, Refused> {
        let mut members = Members::new();
        let mut next = 0u64;
        for (region, count) in &shape.members {
            for _ in 0..*count {
                members.join(NodeId(next), region.clone());
                next = next.checked_add(1).ok_or(Refused::Overflow("members"))?;
            }
        }
        let primary = shape
            .members
            .first()
            .map(|(r, _)| r.clone())
            .ok_or(Refused::TooFewRegions { have: 0, needs: 1 })?;
        let first = NodeId(0);
        let directory = |n| crate::groups::even(first, n).map_err(|_| Refused::NoGroup);
        let mut regions = BTreeMap::new();
        for home in &shape.homes {
            regions.insert(home.clone(), directory(shape.groups)?);
        }
        let atlas = Atlas::new(directory(shape.groups)?, regions);
        let physical = members.regions();
        let mut rules = BTreeMap::new();
        for address in atlas.groups() {
            let rule = match &address.scope {
                Scope::Global => Rule::Global {
                    survive_regions: shape.survive_regions,
                    survive: shape.survive,
                    lead: Some(primary.clone()),
                },
                Scope::Region(home) => Rule::Home {
                    region: if physical.contains(home) {
                        home.clone()
                    } else {
                        primary.clone()
                    },
                    survive: shape.survive,
                },
            };
            rules.insert(address, rule);
        }
        let mut lab = Self {
            members,
            latency,
            homes: shape.homes.clone(),
            atlas,
            rules,
            placements: BTreeMap::new(),
            rows: BTreeMap::new(),
        };
        lab.replace()?;
        Ok(lab)
    }

    /// Every group placed again on the live members, each as close to its last placement as its
    /// rule allows; refused whole if any rule can't be met.
    fn replace(&mut self) -> Result<(), Refused> {
        let mut next = BTreeMap::new();
        for (address, rule) in &self.rules {
            let placed = place(&self.members, rule, self.placements.get(address))?;
            next.insert(address.clone(), placed);
        }
        self.placements = next;
        Ok(())
    }

    /// `node` is lost, and every group placed again.
    pub fn lose(&mut self, node: NodeId) -> Result<(), Refused> {
        self.members.lose(node);
        self.replace()
    }

    /// Every member of `region` is lost: each group is placed again where its rule can still be
    /// met, and keeps its placement where it can't (the root, short of a region, still quorate
    /// on the rest; a regional group homed there, not quorate until the region is restored).
    pub fn lose_region(&mut self, region: &Region) {
        self.members.lose_region(region);
        let mut next = self.placements.clone();
        for (address, rule) in &self.rules {
            if let Ok(placed) = place(&self.members, rule, self.placements.get(address)) {
                next.insert(address.clone(), placed);
            }
        }
        self.placements = next;
    }

    /// The members, as the lab holds them now.
    #[must_use]
    pub fn members(&self) -> &Members {
        &self.members
    }

    /// Every group with its rule and placement now.
    pub fn placements(&self) -> impl Iterator<Item = (&Address, &Rule, &Placement)> + '_ {
        self.rules
            .iter()
            .filter_map(|(a, r)| self.placements.get(a).map(|p| (a, r, p)))
    }

    fn client(&self, client: usize) -> Result<&Region, Refused> {
        let n = self.homes.len();
        client
            .checked_rem(n)
            .and_then(|i| self.homes.get(i))
            .ok_or(Refused::TooFewRegions { have: n, needs: 1 })
    }

    /// The members' region a database region is placed in.
    fn physical(&self, home: &Region) -> Result<Region, Refused> {
        let address = Address {
            scope: Scope::Region(home.clone()),
            group: crate::reshard::GroupId(0),
        };
        match self.rules.get(&address) {
            Some(Rule::Home { region, .. }) => Ok(region.clone()),
            _ => Err(Refused::NoRegion(home.clone())),
        }
    }

    fn lead_region(&self, address: &Address) -> Result<(&Placement, Region), Refused> {
        let p = self.placements.get(address).ok_or(Refused::NoGroup)?;
        let r = self
            .members
            .region_of(p.lead)
            .ok_or(Refused::Unknown(p.lead))?;
        Ok((p, r.clone()))
    }

    /// How long a write from `from` to `address` takes to commit.
    fn commit(&self, from: &Region, address: &Address) -> Result<Duration, Refused> {
        let (p, lead) = self.lead_region(address)?;
        if !quorate(&self.members, p) {
            return Err(Refused::Breaks("the group has no live quorum"));
        }
        let mut acks: Vec<Duration> = Vec::new();
        for v in p
            .voters
            .iter()
            .filter(|v| **v != p.lead && self.members.live(**v))
        {
            let r = self.members.region_of(*v).ok_or(Refused::Unknown(*v))?;
            acks.push(self.latency.rtt(&lead, r));
        }
        acks.sort_unstable();
        // The majority less the lead's own vote: `voters / 2` more acks.
        let wait = p
            .voters
            .len()
            .checked_div(2)
            .and_then(|k| k.checked_sub(1))
            .and_then(|i| acks.get(i))
            .copied()
            .unwrap_or_default();
        self.latency
            .rtt(from, &lead)
            .checked_add(wait)
            .ok_or(Refused::Overflow("commit time"))
    }

    /// Runs `ops` in order.
    pub fn run(&mut self, ops: &[Op]) -> Result<Outcome, Refused> {
        let mut out = Outcome::default();
        for op in ops {
            match op {
                Op::Put {
                    client,
                    key,
                    value,
                    global,
                } => {
                    let home = self.client(*client)?.clone();
                    let from = self.physical(&home)?;
                    let scope = if *global {
                        Scope::Global
                    } else {
                        Scope::Region(home)
                    };
                    let address = self.atlas.write(&scope, key)?;
                    let took = self.commit(&from, &address)?;
                    self.rows
                        .entry(address)
                        .or_default()
                        .insert(key.clone(), *value);
                    if *global {
                        out.global_commits.push(took);
                    } else {
                        out.regional_commits.push(took);
                    }
                }
                Op::Get {
                    client,
                    key,
                    global,
                } => {
                    let home = self.client(*client)?.clone();
                    let tries = if *global {
                        vec![self.atlas.write(&Scope::Global, key)?]
                    } else {
                        self.atlas.read(&home, key)?
                    };
                    let found = tries
                        .iter()
                        .find_map(|a| self.rows.get(a).and_then(|rows| rows.get(key)).copied());
                    out.answers.push(found);
                }
            }
        }
        for (address, rows) in &self.rows {
            for (key, value) in rows {
                out.rows
                    .insert((address.scope.clone(), key.clone()), *value);
            }
        }
        Ok(out)
    }
}

/// The `q`-quantile of `samples` by nearest rank (`q` in thousandths: 990 is p99), none of none.
#[must_use]
pub fn quantile(samples: &[Duration], q: u32) -> Option<Duration> {
    let mut sorted = samples.to_vec();
    sorted.sort_unstable();
    let n = u64::try_from(sorted.len()).ok()?;
    let rank = n.checked_mul(u64::from(q))?.div_ceil(1000).max(1);
    let i = usize::try_from(rank.checked_sub(1)?).ok()?;
    sorted.get(i).copied()
}
