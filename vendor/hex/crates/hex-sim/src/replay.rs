//! A recorded workload, as text: the nodes it lands on and the requests that arrived, each an OCI
//! container, a Firecracker microVM or a Shards microVM, so hex-sim can replay a real day (G2 step 3;
//! `AC-T7.4.3`'s recorded workload, bench/sched/huawei_workload.md).
//!
//! ```text
//! # source: <where and when it was recorded>
//! node,<cpu_milli>,<mem_bytes>,<gpus>,<background_milli>[,<classes>]
//! owner,<weight>
//! group,<budget>
//! overhead,<class>,<mem_bytes>[,<for_mem_bytes>]
//! leg,<arrive_ns>,<runs_for_ns>,<cpu_milli>,<mem_bytes>,<gpus>,<plain|build|benchmark|session>[,<owner>,<band>,<gang|->,<group|->[,<class>]]
//! ```
//!
//! Lines starting `#` are comments, and blank lines are skipped. Every node, owner and group comes
//! before the first leg, and legs come in arrival order. Ids are given by position. A leg's tenancy
//! (G4: its owner, priority band 0-255, gang and disruption group, `-` for none) is all four fields
//! or none; a leg without them is owner 0's, band 0, alone and in no group, and a trace that names no
//! owner has one, of weight 1, so a trace written before G4 reads as it did. A class (`oci`,
//! `firecracker` or `shards`) follows a leg's tenancy, `oci` when absent; a node's classes are
//! joined by `+` (`oci+firecracker`), every class when absent. Nodes use the original trace's Linux
//! x86_64 platform; a declared microVM class implies KVM, which that platform needs to run it.
//! A class's overhead, before the first leg, is the memory its runtime costs a node; each leg of
//! that class needs it beside its own, none
//! when absent. It is one row for the class, or one row per leg memory (`for_mem_bytes`) where it was
//! measured by guest size, never both; then a leg of that class whose memory has no row is refused.
//! The parser is a pure function
//! over bounded input (CLAUDE.md §2): past [`Limits`], and at the first line it can't read, it refuses
//! with a typed [`TraceError`] naming the line.

use crate::census;
use crate::model::{
    Class, Classes, Cluster, GangId, Group, GroupId, Kind, NodeId, NodeSpec, Overheads, Owner,
    OwnerId, Resources, Tenancy, Virt, WorkloadId, WorkloadSpec,
};

/// What a trace may hold. Past any bound the parse is refused, never truncated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_bytes: usize,
    pub max_nodes: usize,
    pub max_legs: usize,
    pub max_owners: usize,
    pub max_groups: usize,
    pub max_overheads: usize,
}

impl Limits {
    /// The input-bound rule ([`crate::census`]): twice the most any committed trace holds, of each.
    pub const RECORDED: Self = Self {
        max_bytes: census::BYTES * census::INPUT_MARGIN,
        max_nodes: census::NODES * census::INPUT_MARGIN,
        max_legs: census::LEGS * census::INPUT_MARGIN,
        max_owners: census::OWNERS * census::INPUT_MARGIN,
        max_groups: census::GROUPS * census::INPUT_MARGIN,
        max_overheads: census::OVERHEADS * census::INPUT_MARGIN,
    };
}

impl Default for Limits {
    /// [`Limits::RECORDED`].
    fn default() -> Self {
        Self::RECORDED
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TraceError {
    #[error("the trace is {0} bytes, over its bound")]
    TooLarge(usize),
    #[error("line {line}: {why}")]
    Line { line: usize, why: &'static str },
    #[error("more than {0} nodes")]
    TooManyNodes(usize),
    #[error("more than {0} legs")]
    TooManyLegs(usize),
    #[error("more than {0} owners")]
    TooManyOwners(usize),
    #[error("more than {0} groups")]
    TooManyGroups(usize),
    #[error("more than {0} overhead rows")]
    TooManyOverheads(usize),
    #[error("the trace names no node")]
    NoNodes,
}

fn bad(line: usize, why: &'static str) -> TraceError {
    TraceError::Line { line, why }
}

pub fn parse(text: &str, limits: &Limits) -> Result<Cluster, TraceError> {
    if text.len() > limits.max_bytes {
        return Err(TraceError::TooLarge(text.len()));
    }
    let mut nodes: Vec<NodeSpec> = Vec::new();
    let mut workloads: Vec<WorkloadSpec> = Vec::new();
    let mut owners: Vec<Owner> = Vec::new();
    let mut groups: Vec<Group> = Vec::new();
    let mut overheads = Overheads::default();
    let mut overheads_seen = Classes::NONE;
    let mut overheads_by_size = Classes::NONE;
    let mut overhead_rows = 0usize;
    for (at, raw) in text.lines().enumerate() {
        let line = at.saturating_add(1);
        let row = raw.trim();
        if row.is_empty() || row.starts_with('#') {
            continue;
        }
        let mut fields = row.split(',').map(str::trim);
        match fields.next() {
            Some("node") => {
                if !workloads.is_empty() {
                    return Err(bad(line, "a node after the first leg"));
                }
                if nodes.len() >= limits.max_nodes {
                    return Err(TraceError::TooManyNodes(limits.max_nodes));
                }
                let id = NodeId(u32::try_from(nodes.len()).map_err(|_| bad(line, "node id"))?);
                let cpu_milli = number(&mut fields, line)?;
                let mem_bytes = number(&mut fields, line)?;
                let gpus = u32::try_from(number(&mut fields, line)?)
                    .map_err(|_| bad(line, "gpus past u32"))?;
                let background_milli = number(&mut fields, line)?;
                let classes = match fields.next() {
                    None => Classes::default(),
                    Some(text) => node_classes(text, line)?,
                };
                end(&mut fields, line)?;
                if cpu_milli == 0 {
                    return Err(bad(line, "a node with no CPU"));
                }
                nodes.push(NodeSpec {
                    routes: None,
                    costs: [None; crate::model::Class::COUNT],
                    priors: Default::default(),
                    runtime_pools: Vec::new(),
                    id,
                    capacity: Resources {
                        cpu_milli,
                        mem_bytes,
                        gpus,
                        disk_bytes: 0,
                    },
                    background_milli,
                    classes,
                    platform: Default::default(),
                    virt: Virt {
                        kvm: classes.has(Class::Firecracker) || classes.has(Class::Shards),
                        ..Virt::default()
                    },
                    devices: crate::model::unnamed_gpus(gpus),
                });
            }
            Some("owner") => {
                if !workloads.is_empty() {
                    return Err(bad(line, "an owner after the first leg"));
                }
                if owners.len() >= limits.max_owners {
                    return Err(TraceError::TooManyOwners(limits.max_owners));
                }
                let weight = u32::try_from(number(&mut fields, line)?)
                    .map_err(|_| bad(line, "a weight past u32"))?;
                end(&mut fields, line)?;
                if weight == 0 {
                    return Err(bad(line, "an owner of weight 0"));
                }
                owners.push(Owner { weight });
            }
            Some("overhead") => {
                if !workloads.is_empty() {
                    return Err(bad(line, "an overhead after the first leg"));
                }
                if overhead_rows >= limits.max_overheads {
                    return Err(TraceError::TooManyOverheads(limits.max_overheads));
                }
                overhead_rows = overhead_rows.saturating_add(1);
                let class = class(fields.next().ok_or(bad(line, "a missing field"))?, line)?;
                let bytes = number(&mut fields, line)?;
                match fields.next() {
                    None => {
                        if overheads_seen.has(class) {
                            return Err(bad(line, "a class's overhead given twice"));
                        }
                        if overheads_by_size.has(class) {
                            return Err(bad(line, "a class's overhead given both by size and not"));
                        }
                        overheads_seen = overheads_seen.with(class);
                        overheads.set(class, bytes);
                    }
                    Some(for_mem) => {
                        let for_mem = whole(for_mem, line)?;
                        end(&mut fields, line)?;
                        if overheads_seen.has(class) {
                            return Err(bad(line, "a class's overhead given both by size and not"));
                        }
                        overheads_by_size = overheads_by_size.with(class);
                        overheads
                            .set_size(class, for_mem, bytes)
                            .map_err(|why| bad(line, why))?;
                    }
                }
            }
            Some("group") => {
                if !workloads.is_empty() {
                    return Err(bad(line, "a group after the first leg"));
                }
                if groups.len() >= limits.max_groups {
                    return Err(TraceError::TooManyGroups(limits.max_groups));
                }
                let budget = u32::try_from(number(&mut fields, line)?)
                    .map_err(|_| bad(line, "a budget past u32"))?;
                end(&mut fields, line)?;
                groups.push(Group { budget });
            }
            Some("leg") => {
                if workloads.len() >= limits.max_legs {
                    return Err(TraceError::TooManyLegs(limits.max_legs));
                }
                let id =
                    WorkloadId(u64::try_from(workloads.len()).map_err(|_| bad(line, "leg id"))?);
                let arrives = number(&mut fields, line)?;
                let runs_for = number(&mut fields, line)?;
                let cpu_milli = number(&mut fields, line)?;
                let mem_bytes = number(&mut fields, line)?;
                let gpus = u32::try_from(number(&mut fields, line)?)
                    .map_err(|_| bad(line, "gpus past u32"))?;
                let kind = match fields.next() {
                    Some("plain") => Kind::Plain,
                    Some("build") => Kind::Build,
                    Some("benchmark") => Kind::Benchmark,
                    Some("session") => Kind::Session,
                    Some(_) => {
                        return Err(bad(line, "a kind not plain, build, benchmark or session"));
                    }
                    None => return Err(bad(line, "a missing field")),
                };
                let tenancy = tenancy(&mut fields, line, owners.len().max(1), groups.len())?;
                let class = match fields.next() {
                    None => Class::default(),
                    Some(text) => class(text, line)?,
                };
                end(&mut fields, line)?;
                let overhead = overheads.of(class, mem_bytes).ok_or(bad(
                    line,
                    "a leg whose memory its class's overhead has no row for",
                ))?;
                let mem_bytes = mem_bytes
                    .checked_add(overhead)
                    .ok_or(bad(line, "memory past u64 with its class's overhead"))?;
                if workloads.last().is_some_and(|w| w.arrives > arrives) {
                    return Err(bad(line, "a leg arriving before the one above it"));
                }
                if runs_for == 0 {
                    return Err(bad(line, "a leg that runs for no time"));
                }
                workloads.push(WorkloadSpec {
                    id,
                    need: Resources {
                        cpu_milli,
                        mem_bytes,
                        gpus,
                        disk_bytes: 0,
                    },
                    kind,
                    arrives,
                    runs_for: Some(runs_for),
                    tenancy,
                    class,
                    ask: Default::default(),
                });
            }
            Some(_) | None => {
                return Err(bad(
                    line,
                    "a line that is not node, owner, group, overhead or leg",
                ));
            }
        }
    }
    if nodes.is_empty() {
        return Err(TraceError::NoNodes);
    }
    if owners.is_empty() {
        owners.push(Owner::default());
    }
    Ok(Cluster {
        nodes,
        workloads,
        models: Vec::new(),
        owners,
        groups,
        overheads,
    })
}

/// A leg's optional trailing tenancy: all four fields, or none. `owners` counts the owner a trace
/// that names none has.
fn tenancy<'a>(
    fields: &mut impl Iterator<Item = &'a str>,
    line: usize,
    owners: usize,
    groups: usize,
) -> Result<Tenancy, TraceError> {
    let Some(owner) = fields.next() else {
        return Ok(Tenancy::default());
    };
    let owner = u32::try_from(whole(owner, line)?).map_err(|_| bad(line, "an owner past u32"))?;
    if !usize::try_from(owner).is_ok_and(|o| o < owners) {
        return Err(bad(line, "a leg naming an owner the trace doesn't"));
    }
    let band = u8::try_from(number(fields, line)?).map_err(|_| bad(line, "a band past 255"))?;
    let gang = match fields.next() {
        Some("-") => None,
        Some(field) => Some(GangId(whole(field, line)?)),
        None => return Err(bad(line, "a missing field")),
    };
    let group = match fields.next() {
        Some("-") => None,
        Some(field) => {
            let group =
                u32::try_from(whole(field, line)?).map_err(|_| bad(line, "a group past u32"))?;
            if !usize::try_from(group).is_ok_and(|g| g < groups) {
                return Err(bad(line, "a leg naming a group the trace doesn't"));
            }
            Some(GroupId(group))
        }
        None => return Err(bad(line, "a missing field")),
    };
    Ok(Tenancy {
        owner: OwnerId(owner),
        band,
        gang,
        group,
    })
}

fn class(text: &str, line: usize) -> Result<Class, TraceError> {
    match text {
        "oci" => Ok(Class::Oci),
        "firecracker" => Ok(Class::Firecracker),
        "shards" => Ok(Class::Shards),
        _ => Err(bad(line, "a class not oci, firecracker or shards")),
    }
}

/// A node's classes, joined by `+`; at least one. At most one per class it could name, so the walk
/// is bounded by the classes there are.
fn node_classes(text: &str, line: usize) -> Result<Classes, TraceError> {
    let mut classes = Classes::NONE;
    for (n, name) in text.split('+').enumerate() {
        if n >= 3 {
            return Err(bad(line, "more classes than there are"));
        }
        classes = classes.with(class(name, line)?);
    }
    Ok(classes)
}

fn number<'a>(fields: &mut impl Iterator<Item = &'a str>, line: usize) -> Result<u64, TraceError> {
    whole(fields.next().ok_or(bad(line, "a missing field"))?, line)
}

fn whole(field: &str, line: usize) -> Result<u64, TraceError> {
    if field.is_empty() || !field.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad(line, "a field that is not a whole number"));
    }
    field
        .parse::<u64>()
        .map_err(|_| bad(line, "a number past u64"))
}

fn end<'a>(fields: &mut impl Iterator<Item = &'a str>, line: usize) -> Result<(), TraceError> {
    match fields.next() {
        None => Ok(()),
        Some(_) => Err(bad(line, "more fields than the line's kind has")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SMALL: &str = "# source: test\nnode,4000,8589934592,0,500\nnode,8000,17179869184,2,0\n\nleg,0,1000,1000,1024,0,build\nleg,10,2000,500,1024,1,plain\nleg,10,500,250,1024,0,session\n";

    /// Do a trace naming each node's classes and each leg's; expect them read as written, and a
    /// node or leg that names none read as every class and as an OCI container.
    #[test]
    fn classes_are_read_and_default_to_every_class_and_oci() {
        let text = "node,8,8,0,0,oci+firecracker\nnode,8,8,0,0\nleg,0,1,1,1,0,plain,0,0,-,-,shards\nleg,1,1,1,1,0,plain\n";
        let c = parse(text, &Limits::default()).unwrap();
        let first = c.nodes[0].classes;
        assert!(
            first.has(Class::Oci) && first.has(Class::Firecracker) && !first.has(Class::Shards)
        );
        assert_eq!(c.nodes[1].classes, Classes::ALL);
        assert_eq!(
            (c.workloads[0].class, c.workloads[1].class),
            (Class::Shards, Class::Oci)
        );
        let with = "node,8,8,0,0\noverhead,shards,5\nleg,0,1,1,1,0,plain,0,0,-,-,shards\nleg,1,1,1,1,0,plain\n";
        let c = parse(with, &Limits::default()).unwrap();
        assert_eq!(
            (c.workloads[0].need.mem_bytes, c.workloads[1].need.mem_bytes),
            (6, 1)
        );
        assert_eq!(c.overheads.of(Class::Shards, 1), Some(5));
    }

    /// AC-T7.5.2: do legacy Linux trace nodes with no class field, OCI alone, Firecracker alone and
    /// Shards alone; expect KVM exactly when a declared class needs it, with capacity and GPUs intact.
    #[test]
    fn ac_t7_5_2_linux_trace_nodes_derive_kvm_from_their_classes() {
        for (classes, expected, kvm) in [
            ("", Classes::ALL, true),
            (",oci", Classes::NONE.with(Class::Oci), false),
            (",firecracker", Classes::NONE.with(Class::Firecracker), true),
            (",shards", Classes::NONE.with(Class::Shards), true),
        ] {
            let c = parse(
                &format!("node,4000,1024,2,500{classes}\n"),
                &Limits::default(),
            )
            .unwrap();
            let node = &c.nodes[0];
            assert_eq!(
                node.virt,
                Virt {
                    kvm,
                    ..Virt::default()
                },
                "{classes}"
            );
            assert_eq!(node.platform, crate::model::Platform::default());
            assert_eq!(
                node.capacity,
                Resources {
                    cpu_milli: 4000,
                    mem_bytes: 1024,
                    gpus: 2,
                    disk_bytes: 0
                }
            );
            assert_eq!(node.background_milli, 500);
            assert_eq!(node.devices, crate::model::unnamed_gpus(2));
            assert_eq!(node.classes, expected);
        }
    }

    /// Do a class's overhead given by guest size (Shards' measured one): expect each leg of the class
    /// to need its own size's overhead beside its memory, the other classes theirs, and a leg whose
    /// memory has no row refused by line.
    #[test]
    fn an_overhead_by_size_charges_each_leg_its_own_size() {
        let text = "node,8,64,0,0
overhead,oci,1
overhead,shards,7,20
overhead,shards,5,10
leg,0,1,1,10,0,plain,0,0,-,-,shards
leg,1,1,1,20,0,plain,0,0,-,-,shards
leg,2,1,1,30,0,plain
";
        let c = parse(text, &Limits::default()).unwrap();
        let needs: Vec<_> = c.workloads.iter().map(|w| w.need.mem_bytes).collect();
        assert_eq!(needs, vec![15, 27, 31]);
        assert_eq!(
            (
                c.overheads.own(Class::Shards, 27),
                c.overheads.own(Class::Shards, 26)
            ),
            (Some(20), None)
        );
        let unmatched = "node,8,64,0,0
overhead,shards,5,10
leg,0,1,1,11,0,plain,0,0,-,-,shards
";
        assert_eq!(
            parse(unmatched, &Limits::default()).unwrap_err(),
            TraceError::Line {
                line: 3,
                why: "a leg whose memory its class's overhead has no row for"
            }
        );
    }

    /// G2 step 3: a well-formed trace reads back as its nodes and legs, ids by position.
    #[test]
    fn a_trace_reads_as_its_cluster_and_legs() {
        let c = parse(SMALL, &Limits::default()).unwrap();
        assert_eq!(c.nodes.len(), 2);
        assert_eq!(c.nodes[1].capacity.gpus, 2);
        assert_eq!(c.nodes[0].background_milli, 500);
        assert_eq!(c.workloads.len(), 3);
        assert_eq!(
            (c.workloads[2].id, c.workloads[2].kind),
            (WorkloadId(2), Kind::Session)
        );
        assert_eq!(c.workloads[1].runs_for, Some(2000));
    }

    /// CLAUDE.md §2 and §6 (hostile input): each malformed trace is refused, typed, naming its line.
    #[test]
    fn malformed_traces_are_refused_by_line() {
        let cases = [
            (
                "node,1,1,0,0\nleg,5,1,1,1,0,plain\nleg,4,1,1,1,0,plain\n",
                3,
            ),
            ("node,1,1,0,0\nleg,0,1,1,1,0,plain\nnode,1,1,0,0\n", 3),
            ("node,1,1,0,0,oci+kata\n", 1),
            ("node,1,1,0,0,\n", 1),
            ("node,1,1,0,0,oci+oci+oci+oci\n", 1),
            ("node,1,1,0,0\nleg,0,1,1,1,0,plain,0,0,-,-,gvisor\n", 2),
            ("node,1,1,0,0\nleg,0,1,1,1,0,plain,0,0,-,-,oci,1\n", 2),
            ("node,1,1,0,0\noverhead,oci,1\noverhead,oci,2\n", 3),
            ("node,1,1,0,0\noverhead,oci,1\noverhead,oci,2,10\n", 3),
            ("node,1,1,0,0\noverhead,oci,2,10\noverhead,oci,1\n", 3),
            ("node,1,1,0,0\noverhead,oci,2,10\noverhead,oci,3,10\n", 3),
            ("node,1,1,0,0\noverhead,oci,2,10,7\n", 2),
            ("node,1,1,0,0\noverhead,oci,2,ten\n", 2),
            ("node,1,1,0,0\nleg,0,1,1,1,0,plain\noverhead,oci,1\n", 3),
            ("node,1,1,0,0\noverhead,lxc,1\n", 2),
            (
                "node,1,1,0,0\noverhead,oci,1\nleg,0,1,1,18446744073709551615,0,plain\n",
                3,
            ),
            ("node,1,1,0\n", 1),
            ("node,1,1,0,0,9\n", 1),
            ("node,-1,1,0,0\n", 1),
            ("node,1,1,99999999999,0\n", 1),
            ("node,1,1,0,0\nleg,0,1,1,1,0,daemon\n", 2),
            ("node,1,1,0,0\nleg,0,0,1,1,0,plain\n", 2),
            ("node,0,1,0,0\n", 1),
            ("pod,1\n", 1),
            ("node,18446744073709551616,1,0,0\n", 1),
        ];
        for (text, line) in cases {
            match parse(text, &Limits::default()) {
                Err(TraceError::Line { line: l, .. }) => assert_eq!(l, line, "{text:?}"),
                other => panic!("{text:?}: {other:?}"),
            }
        }
        assert_eq!(
            parse("# nothing\n", &Limits::default()),
            Err(TraceError::NoNodes)
        );
    }

    const TENANTS: &str = "node,4000,1024,0,0\nowner,1\nowner,3\ngroup,2\nleg,0,1000,1000,1,0,build,1,2,7,0\nleg,5,1000,1000,1,0,build,1,2,7,-\nleg,9,1000,500,1,0,plain,0,0,-,-\n";

    /// AC-T7.5.1 and AC-T7.5.2 (G4 step 1): owners, groups and each leg's owner, band, gang and group
    /// read back; a trace without them reads as one owner of weight 1, every leg its default tenancy.
    #[test]
    fn a_trace_reads_its_owners_groups_and_tenancy() {
        // Its one group: no committed trace names one, so the default bound is none.
        let c = parse(
            TENANTS,
            &Limits {
                max_groups: 1,
                ..Limits::default()
            },
        )
        .unwrap();
        assert_eq!(c.owners, vec![Owner { weight: 1 }, Owner { weight: 3 }]);
        assert_eq!(c.groups, vec![Group { budget: 2 }]);
        assert_eq!(
            c.workloads[0].tenancy,
            Tenancy {
                owner: OwnerId(1),
                band: 2,
                gang: Some(GangId(7)),
                group: Some(GroupId(0)),
            }
        );
        assert_eq!(c.workloads[1].tenancy.group, None);
        assert_eq!(c.workloads[2].tenancy, Tenancy::default());
        let old = parse(SMALL, &Limits::default()).unwrap();
        assert_eq!(old.owners, vec![Owner::default()]);
        assert!(old.groups.is_empty());
        assert!(
            old.workloads
                .iter()
                .all(|w| w.tenancy == Tenancy::default())
        );
    }

    /// CLAUDE.md §6 (hostile input), G4 step 1: each malformed owner, group or tenancy field is
    /// refused, typed, naming its line.
    #[test]
    fn malformed_tenancy_is_refused_by_line() {
        let cases = [
            ("node,1,1,0,0\nowner,0\n", 2),
            ("node,1,1,0,0\nowner,4294967296\n", 2),
            ("node,1,1,0,0\nowner,1,1\n", 2),
            ("node,1,1,0,0\ngroup\n", 2),
            ("node,1,1,0,0\nleg,0,1,1,1,0,plain\nowner,1\n", 3),
            ("node,1,1,0,0\nleg,0,1,1,1,0,plain\ngroup,1\n", 3),
            ("node,1,1,0,0\nleg,0,1,1,1,0,plain,0\n", 2),
            ("node,1,1,0,0\nleg,0,1,1,1,0,plain,0,0,-\n", 2),
            ("node,1,1,0,0\nleg,0,1,1,1,0,plain,1,0,-,-\n", 2),
            ("node,1,1,0,0\nleg,0,1,1,1,0,plain,0,256,-,-\n", 2),
            ("node,1,1,0,0\nleg,0,1,1,1,0,plain,0,0,x,-\n", 2),
            ("node,1,1,0,0\nleg,0,1,1,1,0,plain,0,0,-,0\n", 2),
            ("node,1,1,0,0\nleg,0,1,1,1,0,plain,0,0,-,-,9\n", 2),
        ];
        // Room for each case's one group, so each is refused for its line.
        let room = Limits {
            max_groups: 1,
            ..Limits::default()
        };
        for (text, line) in cases {
            match parse(text, &room) {
                Err(TraceError::Line { line: l, .. }) => assert_eq!(l, line, "{text:?}"),
                other => panic!("{text:?}: {other:?}"),
            }
        }
        let limits = Limits {
            max_owners: 1,
            max_groups: 1,
            ..Limits::default()
        };
        assert_eq!(
            parse("node,1,1,0,0\nowner,1\nowner,1\n", &limits),
            Err(TraceError::TooManyOwners(1))
        );
        assert_eq!(
            parse("node,1,1,0,0\ngroup,1\ngroup,1\n", &limits),
            Err(TraceError::TooManyGroups(1))
        );
    }

    #[test]
    fn traces_past_their_bounds_are_refused() {
        let limits = Limits {
            max_bytes: 10,
            max_nodes: 1,
            max_legs: 1,
            ..Limits::default()
        };
        assert_eq!(
            parse(SMALL, &limits),
            Err(TraceError::TooLarge(SMALL.len()))
        );
        let limits = Limits {
            max_bytes: 1 << 20,
            ..limits
        };
        assert_eq!(parse(SMALL, &limits), Err(TraceError::TooManyNodes(1)));
        let one_node = "node,1,1,0,0\nleg,0,1,1,1,0,plain\nleg,0,1,1,1,0,plain\n";
        assert_eq!(parse(one_node, &limits), Err(TraceError::TooManyLegs(1)));
        // The input-bound rule's: twice the census's most overhead rows.
        let most = crate::census::OVERHEADS * crate::census::INPUT_MARGIN;
        let mut sizes = String::from("node,1,1,0,0\n");
        for size in 0..most {
            sizes.push_str(&format!("overhead,shards,1,{size}\n"));
        }
        assert!(parse(&sizes, &Limits::default()).is_ok());
        sizes.push_str(&format!("overhead,shards,1,{most}\n"));
        assert_eq!(
            parse(&sizes, &Limits::default()),
            Err(TraceError::TooManyOverheads(most))
        );
    }

    proptest::proptest! {
        /// Hostile input: any bytes at all are read or refused, never a panic.
        #[test]
        fn any_text_is_read_or_refused(text in "\\PC*") {
            let _ = parse(&text, &Limits::default());
        }

        /// Hostile input shaped like a trace: lines of the right words and numbers in any order.
        #[test]
        fn any_trace_shaped_text_is_read_or_refused(
            rows in proptest::collection::vec("(node|leg|owner|group|#|x)(,[0-9]{0,22}){0,7}(,(plain|build|benchmark|session|zz))?(,([0-9]{0,11}|-)){0,5}", 0..40)
        ) {
            let _ = parse(&rows.join("\n"), &Limits::default());
        }
    }
}
