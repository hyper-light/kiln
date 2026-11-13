//! The control plane's durable state: one record per workload and each node's ledger, the kernel's
//! (`crate::lb::Runner`, kiln's `lb/runner.rs`: a placement is admitted by its node's ledger, never
//! past its budget), changed only by compare-and-set writes (`scheduler-architecture.md` S6, Omega's optimistic commit
//! CM[2]). A placement names the record version it read, so a commit made twice, or made from a
//! stale read, is a conflict and never a second start. A write is atomic: the nemesis crashes the
//! control plane before one is applied or after it is applied and before anyone hears of it.
//!
//! The store outlives control-plane crashes, as Amphora does for the real control plane. Its log of
//! applied writes is the history the serial specification checks ([`crate::spec`]).

use std::collections::BTreeMap;

use crate::error::SimError;
use crate::lb;
use crate::model::{
    Device, Group, GroupId, Held, NodeId, NodeSpec, Resources, Time, WorkloadId, WorkloadSpec, bind,
};

/// The plane, as the owner its nodes' ledgers know it by.
const PLANE: lb::OwnerId = lb::OwnerId(0);

/// A need as the kernel's amounts: CPU, memory, disk (none here), GPUs.
fn amounts(r: &Resources) -> lb::Amounts {
    [r.cpu_milli, r.mem_bytes, r.disk_bytes, u64::from(r.gpus)]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Pending,
    Placed(NodeId),
    Complete(NodeId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Record {
    pub spec: WorkloadSpec,
    /// The order the store accepted it in: the serial specification's queue order.
    pub seq: u64,
    pub version: u64,
    pub state: State,
    /// Times it was placed: each placement starts it once, so a preempted workload placed again
    /// starts again without a double start.
    pub placements: u32,
    /// Preempted and not placed again: one of its group's disruptions (`AC-T7.5.3`).
    pub preempted: bool,
    /// The write that last placed it: the newer of two victims has the larger.
    pub placed_write: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Write {
    Submit(WorkloadSpec),
    Place {
        workload: WorkloadId,
        node: NodeId,
        version: u64,
    },
    Complete {
        workload: WorkloadId,
        node: NodeId,
        version: u64,
    },
}

/// An applied write, as the log keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Submit {
        workload: WorkloadId,
        seq: u64,
    },
    Place {
        workload: WorkloadId,
        node: NodeId,
    },
    Complete {
        workload: WorkloadId,
        node: NodeId,
    },
    /// Taken off its node and back to pending, so a higher band or a lower share can have the room.
    Preempt {
        workload: WorkloadId,
        node: NodeId,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub at: Time,
    /// The applied write it is part of, counted from 0: a gang's placements share one.
    pub write: u64,
    pub op: Op,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Conflict {
    /// The record changed since the writer read it.
    Version { read: u64, found: u64 },
    /// The record isn't in the state the write moves it from.
    State(State),
    /// The node hasn't the room.
    NodeFull,
    /// No such record.
    Missing,
    /// Preempting it would put its group past its disruption budget.
    Budget(GroupId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    Applied {
        op: Op,
        version: u64,
    },
    /// The write was already applied (a client's resubmit, a node's repeated report): nothing changes.
    Duplicate {
        version: u64,
    },
    Conflict(Conflict),
}

/// A preemption's write: every victim back to pending and the preemptor placed, with their new
/// versions, or nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreemptOutcome {
    Applied { victims: Vec<u64>, placed: u64 },
    Conflict(WorkloadId, Conflict),
}

/// A gang's write: every member placed, with its new version in the order given, or none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GangOutcome {
    Applied { versions: Vec<u64> },
    Conflict(WorkloadId, Conflict),
}

#[derive(Debug, Clone)]
pub struct Store {
    /// Each node's ledger, by node id: what admits a placement and frees its room.
    nodes: Vec<lb::Runner>,
    /// Each node's devices, by node id, and the workload holding each until it is deleted: a node's
    /// ledger holds them beside its room.
    devices: Vec<Vec<Device>>,
    held: Vec<Vec<Option<WorkloadId>>>,
    /// Each placed workload's handle in its node's ledger.
    boxes: BTreeMap<WorkloadId, lb::Handle>,
    records: BTreeMap<WorkloadId, Record>,
    next_seq: u64,
    log: Vec<Entry>,
    /// Writes applied so far.
    writes: u64,
    /// Each group's members preempted and not placed again; a group with none has no entry.
    disrupted: BTreeMap<GroupId, u32>,
    max_records: usize,
    max_log: usize,
}

impl Store {
    /// `max_records` bounds the workloads it keeps and `max_log` its history; past either a write is
    /// refused with [`SimError::Full`].
    pub fn new(nodes: &[NodeSpec], max_records: usize, max_log: usize) -> Result<Self, SimError> {
        for (at, node) in nodes.iter().enumerate() {
            if usize::try_from(node.id.0).ok() != Some(at) {
                return Err(SimError::Config("node ids must be dense from 0, in order"));
            }
            if !node.devices_agree() {
                return Err(SimError::Config(
                    "a node's GPUs listed aren't its capacity's, or it lists more devices than a PCI segment has",
                ));
            }
        }
        Ok(Self {
            nodes: nodes
                .iter()
                .map(|n| lb::Runner::new(lb::RunnerId(n.id.0), amounts(&n.capacity), 1))
                .collect(),
            devices: nodes.iter().map(|n| n.devices.clone()).collect(),
            held: nodes.iter().map(|n| vec![None; n.devices.len()]).collect(),
            boxes: BTreeMap::new(),
            records: BTreeMap::new(),
            next_seq: 0,
            log: Vec::new(),
            writes: 0,
            disrupted: BTreeMap::new(),
            max_records,
            max_log,
        })
    }

    /// Admits `workload` in `node`'s ledger: its room, and the devices it asks ([`bind`]), held by it
    /// until it is deleted. False, changing nothing, when either isn't free.
    fn admit(&mut self, node: NodeId, workload: WorkloadId) -> Result<bool, SimError> {
        let spec = self
            .records
            .get(&workload)
            .ok_or(SimError::UnknownWorkload(workload))?
            .spec;
        let at = usize::try_from(node.0).map_err(|_| SimError::UnknownNode(node))?;
        let (devices, held) = (
            self.devices.get(at).ok_or(SimError::UnknownNode(node))?,
            self.held.get(at).ok_or(SimError::UnknownNode(node))?,
        );
        let Some(bound) = bind(
            devices,
            |i| held.get(i).is_some_and(Option::is_none),
            spec.need.gpus,
            &spec.ask,
        ) else {
            return Ok(false);
        };
        let runner = self.nodes.get_mut(at).ok_or(SimError::UnknownNode(node))?;
        let start = lb::Start {
            owner: PLANE,
            run: lb::Handle::NONE,
            epoch: runner.epoch(),
            need: amounts(&spec.need),
        };
        let lb::ToOwner::Started { at: handle, .. } = runner.start(start) else {
            return Ok(false);
        };
        self.boxes.insert(workload, handle);
        if let Some(hs) = self.held.get_mut(at) {
            for i in bound {
                if let Some(h) = hs.get_mut(usize::from(i)) {
                    *h = Some(workload);
                }
            }
        }
        Ok(true)
    }

    /// Frees `workload`'s room and devices in `node`'s ledger.
    fn release(&mut self, node: NodeId, workload: WorkloadId) -> Result<(), SimError> {
        let at = self
            .boxes
            .remove(&workload)
            .ok_or(SimError::Resources(node))?;
        let i = usize::try_from(node.0).map_err(|_| SimError::UnknownNode(node))?;
        let runner = self.nodes.get_mut(i).ok_or(SimError::UnknownNode(node))?;
        let mut out = lb::RunnerOut::new();
        runner.end(at, &mut out).ok_or(SimError::Resources(node))?;
        for h in self.held.get_mut(i).into_iter().flatten() {
            if *h == Some(workload) {
                *h = None;
            }
        }
        Ok(())
    }

    /// The devices `workload` holds on `node`, by their places in its devices.
    pub fn holds(&self, node: NodeId, workload: WorkloadId) -> Held {
        let at = usize::try_from(node.0).unwrap_or(usize::MAX);
        self.held
            .get(at)
            .into_iter()
            .flatten()
            .zip(0u16..)
            .filter(|(h, _)| **h == Some(workload))
            .map(|(_, i)| i)
            .collect()
    }

    pub fn apply(&mut self, at: Time, write: Write) -> Result<Outcome, SimError> {
        let outcome = match write {
            Write::Submit(spec) => self.submit(spec)?,
            Write::Place {
                workload,
                node,
                version,
            } => self.place(workload, node, version)?,
            Write::Complete {
                workload,
                node,
                version,
            } => self.complete(workload, node, version)?,
        };
        if let Outcome::Applied { op, .. } = outcome {
            if self.log.len() >= self.max_log {
                return Err(SimError::Full {
                    what: "the store's log",
                    bound: self.max_log,
                });
            }
            self.log.push(Entry {
                at,
                write: self.writes,
                op,
            });
            self.writes = self.writes.checked_add(1).ok_or(SimError::Full {
                what: "the store's writes",
                bound: usize::MAX,
            })?;
        }
        Ok(outcome)
    }

    /// Places every member of a gang in one write, or none (`AC-T7.5.1`, Kueue's all-or-nothing
    /// admission): each must be pending at the version its writer read, and its node must hold it
    /// beside everything else the write puts there.
    pub fn apply_gang(
        &mut self,
        at: Time,
        members: &[(WorkloadId, NodeId, u64)],
    ) -> Result<GangOutcome, SimError> {
        if self.log.len().saturating_add(members.len()) > self.max_log {
            return Err(SimError::Full {
                what: "the store's log",
                bound: self.max_log,
            });
        }
        for (i, &(workload, _, read)) in members.iter().enumerate() {
            let Some(record) = self.records.get(&workload) else {
                return Ok(GangOutcome::Conflict(workload, Conflict::Missing));
            };
            if record.version != read {
                return Ok(GangOutcome::Conflict(
                    workload,
                    Conflict::Version {
                        read,
                        found: record.version,
                    },
                ));
            }
            if record.state != State::Pending || members.iter().take(i).any(|m| m.0 == workload) {
                return Ok(GangOutcome::Conflict(
                    workload,
                    Conflict::State(record.state),
                ));
            }
        }
        // Each member admitted by its node's ledger, in order; one that doesn't fit beside those
        // before it takes them out again and refuses the write.
        for (i, &(workload, node, _)) in members.iter().enumerate() {
            if !self.admit(node, workload)? {
                for &(before, on, _) in members.iter().take(i) {
                    self.release(on, before)?;
                }
                return Ok(GangOutcome::Conflict(workload, Conflict::NodeFull));
            }
        }
        let mut versions = Vec::with_capacity(members.len());
        for &(workload, node, _) in members {
            let record = self
                .records
                .get_mut(&workload)
                .ok_or(SimError::UnknownWorkload(workload))?;
            record.version = bump(record.version)?;
            record.state = State::Placed(node);
            versions.push(record.version);
            let (group, was) = placed_again(record, self.writes)?;
            end_disruption(&mut self.disrupted, group, was);
            self.log.push(Entry {
                at,
                write: self.writes,
                op: Op::Place { workload, node },
            });
        }
        self.writes = self.writes.checked_add(1).ok_or(SimError::Full {
            what: "the store's writes",
            bound: usize::MAX,
        })?;
        Ok(GangOutcome::Applied { versions })
    }

    fn submit(&mut self, spec: WorkloadSpec) -> Result<Outcome, SimError> {
        if let Some(record) = self.records.get(&spec.id) {
            return Ok(Outcome::Duplicate {
                version: record.version,
            });
        }
        if self.records.len() >= self.max_records || self.log.len() >= self.max_log {
            return Err(SimError::Full {
                what: "the store's records",
                bound: self.max_records,
            });
        }
        let seq = self.next_seq;
        self.next_seq = seq.checked_add(1).ok_or(SimError::Full {
            what: "submit sequence",
            bound: usize::MAX,
        })?;
        self.records.insert(
            spec.id,
            Record {
                spec,
                seq,
                version: 1,
                state: State::Pending,
                placements: 0,
                preempted: false,
                placed_write: 0,
            },
        );
        Ok(Outcome::Applied {
            op: Op::Submit {
                workload: spec.id,
                seq,
            },
            version: 1,
        })
    }

    fn place(
        &mut self,
        workload: WorkloadId,
        node: NodeId,
        read: u64,
    ) -> Result<Outcome, SimError> {
        let Some(record) = self.records.get(&workload) else {
            return Ok(Outcome::Conflict(Conflict::Missing));
        };
        if record.version != read {
            return Ok(Outcome::Conflict(Conflict::Version {
                read,
                found: record.version,
            }));
        }
        if record.state != State::Pending {
            return Ok(Outcome::Conflict(Conflict::State(record.state)));
        }
        let version = bump(record.version)?;
        if self.log.len() >= self.max_log {
            return Err(SimError::Full {
                what: "the store's log",
                bound: self.max_log,
            });
        }
        if !self.admit(node, workload)? {
            return Ok(Outcome::Conflict(Conflict::NodeFull));
        }
        let record = self
            .records
            .get_mut(&workload)
            .ok_or(SimError::UnknownWorkload(workload))?;
        record.version = version;
        record.state = State::Placed(node);
        let (group, was) = placed_again(record, self.writes)?;
        end_disruption(&mut self.disrupted, group, was);
        Ok(Outcome::Applied {
            op: Op::Place { workload, node },
            version,
        })
    }

    fn complete(
        &mut self,
        workload: WorkloadId,
        node: NodeId,
        read: u64,
    ) -> Result<Outcome, SimError> {
        let Some(record) = self.records.get(&workload) else {
            return Ok(Outcome::Conflict(Conflict::Missing));
        };
        if record.state == State::Complete(node) {
            return Ok(Outcome::Duplicate {
                version: record.version,
            });
        }
        if record.state != State::Placed(node) {
            return Ok(Outcome::Conflict(Conflict::State(record.state)));
        }
        if record.version != read {
            return Ok(Outcome::Conflict(Conflict::Version {
                read,
                found: record.version,
            }));
        }
        let version = bump(record.version)?;
        if self.log.len() >= self.max_log {
            return Err(SimError::Full {
                what: "the store's log",
                bound: self.max_log,
            });
        }
        self.release(node, workload)?;
        let record = self
            .records
            .get_mut(&workload)
            .ok_or(SimError::UnknownWorkload(workload))?;
        record.version = version;
        record.state = State::Complete(node);
        Ok(Outcome::Applied {
            op: Op::Complete { workload, node },
            version,
        })
    }

    /// Preempts `victims` and places `head`, in one write, or refuses with nothing changed
    /// (`AC-T7.5.3`): each victim must be placed on the node named at the version its writer read,
    /// the head pending at its read version, no group of `groups` (by id) may end the write with
    /// more members disrupted than its budget (a group the list doesn't name has a budget of 0),
    /// and the head's node must hold it once the victims there are gone.
    pub fn apply_preempt(
        &mut self,
        at: Time,
        victims: &[(WorkloadId, NodeId, u64)],
        head: (WorkloadId, NodeId, u64),
        groups: &[Group],
    ) -> Result<PreemptOutcome, SimError> {
        let entries = victims.len().saturating_add(1);
        if self.log.len().saturating_add(entries) > self.max_log {
            return Err(SimError::Full {
                what: "the store's log",
                bound: self.max_log,
            });
        }
        let mut more: BTreeMap<GroupId, u32> = BTreeMap::new();
        for (i, &(workload, node, read)) in victims.iter().enumerate() {
            let Some(record) = self.records.get(&workload) else {
                return Ok(PreemptOutcome::Conflict(workload, Conflict::Missing));
            };
            if record.version != read {
                return Ok(PreemptOutcome::Conflict(
                    workload,
                    Conflict::Version {
                        read,
                        found: record.version,
                    },
                ));
            }
            if record.state != State::Placed(node)
                || victims.iter().take(i).any(|v| v.0 == workload)
            {
                return Ok(PreemptOutcome::Conflict(
                    workload,
                    Conflict::State(record.state),
                ));
            }
            if let Some(g) = record.spec.tenancy.group {
                let n = more.entry(g).or_default();
                *n = n.checked_add(1).ok_or(SimError::Config("disruptions"))?;
                let budget = usize::try_from(g.0)
                    .ok()
                    .and_then(|at| groups.get(at))
                    .map_or(0, |g| g.budget);
                let now = self.disrupted.get(&g).copied().unwrap_or(0);
                if now.checked_add(*n).is_none_or(|total| total > budget) {
                    return Ok(PreemptOutcome::Conflict(workload, Conflict::Budget(g)));
                }
            }
        }
        let (id, node, read) = head;
        let Some(record) = self.records.get(&id) else {
            return Ok(PreemptOutcome::Conflict(id, Conflict::Missing));
        };
        if record.version != read {
            return Ok(PreemptOutcome::Conflict(
                id,
                Conflict::Version {
                    read,
                    found: record.version,
                },
            ));
        }
        if record.state != State::Pending {
            return Ok(PreemptOutcome::Conflict(id, Conflict::State(record.state)));
        }
        // The victims' room freed in their nodes' ledgers, then the head admitted on its node; a head
        // that doesn't fit even so admits the victims again and refuses the write.
        for &(workload, on, _) in victims {
            self.release(on, workload)?;
        }
        if !self.admit(node, id)? {
            for &(workload, on, _) in victims {
                if !self.admit(on, workload)? {
                    return Err(SimError::Resources(on));
                }
            }
            return Ok(PreemptOutcome::Conflict(id, Conflict::NodeFull));
        }
        let mut versions = Vec::with_capacity(victims.len());
        for &(workload, node, _) in victims {
            let record = self
                .records
                .get_mut(&workload)
                .ok_or(SimError::UnknownWorkload(workload))?;
            record.version = bump(record.version)?;
            record.state = State::Pending;
            record.preempted = true;
            versions.push(record.version);
            if let Some(g) = record.spec.tenancy.group {
                let n = self.disrupted.entry(g).or_default();
                *n = n.checked_add(1).ok_or(SimError::Config("disruptions"))?;
            }
            self.log.push(Entry {
                at,
                write: self.writes,
                op: Op::Preempt { workload, node },
            });
        }
        let record = self
            .records
            .get_mut(&id)
            .ok_or(SimError::UnknownWorkload(id))?;
        record.version = bump(record.version)?;
        record.state = State::Placed(node);
        let placed = record.version;
        let (group, was) = placed_again(record, self.writes)?;
        end_disruption(&mut self.disrupted, group, was);
        self.log.push(Entry {
            at,
            write: self.writes,
            op: Op::Place { workload: id, node },
        });
        self.writes = self.writes.checked_add(1).ok_or(SimError::Full {
            what: "the store's writes",
            bound: usize::MAX,
        })?;
        Ok(PreemptOutcome::Applied {
            victims: versions,
            placed,
        })
    }

    /// `group`'s members preempted and not placed again.
    pub fn disrupted(&self, group: GroupId) -> u32 {
        self.disrupted.get(&group).copied().unwrap_or(0)
    }

    pub fn record(&self, workload: WorkloadId) -> Option<&Record> {
        self.records.get(&workload)
    }

    pub fn records(&self) -> impl Iterator<Item = &Record> {
        self.records.values()
    }

    pub fn log(&self) -> &[Entry] {
        &self.log
    }

    /// The bytes one workload's record takes in the store's map, by its in-memory layout: the key
    /// and the record. The map's node overhead (amortised over its B-tree's entries) is not counted;
    /// `AC-T7.11.4`'s accounting, implementation goal 6, measures the heap.
    pub fn record_bytes() -> usize {
        size_of::<(WorkloadId, Record)>()
    }
}

/// Counts a placement of `record` at write `write` and ends its disruption: its group, and whether
/// it was disrupted.
fn placed_again(record: &mut Record, write: u64) -> Result<(Option<GroupId>, bool), SimError> {
    record.placements = record
        .placements
        .checked_add(1)
        .ok_or(SimError::Config("placements"))?;
    record.placed_write = write;
    let was = record.preempted;
    record.preempted = false;
    Ok((record.spec.tenancy.group, was))
}

fn end_disruption(disrupted: &mut BTreeMap<GroupId, u32>, group: Option<GroupId>, was: bool) {
    let Some(g) = group.filter(|_| was) else {
        return;
    };
    if let Some(n) = disrupted.get_mut(&g) {
        *n = n.saturating_sub(1);
        if *n == 0 {
            disrupted.remove(&g);
        }
    }
}

fn bump(version: u64) -> Result<u64, SimError> {
    version.checked_add(1).ok_or(SimError::Full {
        what: "a record's version",
        bound: usize::MAX,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        let nodes = [NodeSpec {
            routes: None,
            costs: [None; crate::model::Class::COUNT],
            priors: Default::default(),
            runtime_pools: Vec::new(),
            id: NodeId(0),
            capacity: Resources {
                cpu_milli: 1000,
                mem_bytes: 10,
                gpus: 0,
                disk_bytes: 0,
            },
            background_milli: 0,
            classes: crate::model::Classes::ALL,
            platform: Default::default(),
            virt: Default::default(),
            devices: Vec::new(),
        }];
        Store::new(&nodes, 4, 16).unwrap()
    }

    fn spec(id: u64, cpu: u64) -> WorkloadSpec {
        WorkloadSpec {
            id: WorkloadId(id),
            need: Resources {
                cpu_milli: cpu,
                mem_bytes: 1,
                gpus: 0,
                disk_bytes: 0,
            },
            kind: crate::model::Kind::Plain,
            arrives: 0,
            runs_for: None,
            tenancy: crate::model::Tenancy::default(),
            class: crate::model::Class::Oci,
            ask: Default::default(),
        }
    }

    /// AC-T7.3.3: a placement committed twice from the same read starts once; the second is a conflict.
    #[test]
    fn a_placement_from_a_stale_read_conflicts() {
        let mut s = store();
        assert!(matches!(
            s.apply(0, Write::Submit(spec(1, 100))).unwrap(),
            Outcome::Applied { version: 1, .. }
        ));
        let place = Write::Place {
            workload: WorkloadId(1),
            node: NodeId(0),
            version: 1,
        };
        assert!(matches!(
            s.apply(1, place).unwrap(),
            Outcome::Applied { version: 2, .. }
        ));
        assert_eq!(
            s.apply(2, place).unwrap(),
            Outcome::Conflict(Conflict::Version { read: 1, found: 2 })
        );
        assert_eq!(s.log().len(), 2);
    }

    /// AC-T7.3.3: a resubmit and a repeated completion report change nothing.
    #[test]
    fn resubmits_and_repeated_reports_are_duplicates() {
        let mut s = store();
        s.apply(0, Write::Submit(spec(1, 100))).unwrap();
        assert_eq!(
            s.apply(1, Write::Submit(spec(1, 100))).unwrap(),
            Outcome::Duplicate { version: 1 }
        );
        s.apply(
            2,
            Write::Place {
                workload: WorkloadId(1),
                node: NodeId(0),
                version: 1,
            },
        )
        .unwrap();
        let done = Write::Complete {
            workload: WorkloadId(1),
            node: NodeId(0),
            version: 2,
        };
        assert!(matches!(s.apply(3, done).unwrap(), Outcome::Applied { .. }));
        assert_eq!(s.apply(4, done).unwrap(), Outcome::Duplicate { version: 3 });
        assert_eq!(
            s.record(WorkloadId(1)).unwrap().state,
            State::Complete(NodeId(0))
        );
    }

    /// AC-T7.5.1 (gangs whole): a gang the node can't hold together is refused and changes nothing;
    /// one it can is placed in one write, every member's log entry under that write.
    #[test]
    fn a_gang_is_placed_whole_or_not_at_all() {
        let mut s = store();
        for id in 0..3 {
            s.apply(0, Write::Submit(spec(id, 400))).unwrap();
        }
        let gang = |ids: &[u64]| -> Vec<(WorkloadId, NodeId, u64)> {
            ids.iter().map(|&i| (WorkloadId(i), NodeId(0), 1)).collect()
        };
        assert_eq!(
            s.apply_gang(1, &gang(&[0, 1, 2])).unwrap(),
            GangOutcome::Conflict(WorkloadId(2), Conflict::NodeFull)
        );
        assert!(s.records().all(|r| r.state == State::Pending));
        assert_eq!(s.log().len(), 3);
        assert_eq!(
            s.apply_gang(1, &gang(&[0, 0])).unwrap(),
            GangOutcome::Conflict(WorkloadId(0), Conflict::State(State::Pending))
        );
        assert_eq!(
            s.apply_gang(2, &gang(&[0, 1])).unwrap(),
            GangOutcome::Applied {
                versions: vec![2, 2]
            }
        );
        let placed: Vec<_> = s.log()[3..].iter().map(|e| (e.write, e.op)).collect();
        assert_eq!(
            placed,
            vec![
                (
                    3,
                    Op::Place {
                        workload: WorkloadId(0),
                        node: NodeId(0)
                    }
                ),
                (
                    3,
                    Op::Place {
                        workload: WorkloadId(1),
                        node: NodeId(0)
                    }
                ),
            ]
        );
        assert_eq!(
            s.apply_gang(3, &gang(&[2])).unwrap(),
            GangOutcome::Conflict(WorkloadId(2), Conflict::NodeFull)
        );
    }

    /// AC-T7.5.3 (disruption budgets): a preemption that would put a group past its budget is
    /// refused and changes nothing; one within it moves its victims back to pending, marked
    /// disrupted, and places the preemptor, in one write. Placing a disrupted workload again ends
    /// its disruption.
    #[test]
    fn a_preemption_keeps_its_groups_budget() {
        let mut s = store();
        let grouped = |id, cpu| WorkloadSpec {
            tenancy: crate::model::Tenancy {
                group: Some(GroupId(0)),
                ..crate::model::Tenancy::default()
            },
            ..spec(id, cpu)
        };
        let budget = [Group { budget: 1 }];
        for id in 0..2 {
            s.apply(0, Write::Submit(grouped(id, 400))).unwrap();
            s.apply(
                0,
                Write::Place {
                    workload: WorkloadId(id),
                    node: NodeId(0),
                    version: 1,
                },
            )
            .unwrap();
        }
        s.apply(0, Write::Submit(spec(2, 1000))).unwrap();
        let victims = [(WorkloadId(0), NodeId(0), 2), (WorkloadId(1), NodeId(0), 2)];
        let head = (WorkloadId(2), NodeId(0), 1);
        let log = s.log().len();
        assert_eq!(
            s.apply_preempt(1, &victims, head, &budget).unwrap(),
            PreemptOutcome::Conflict(WorkloadId(1), Conflict::Budget(GroupId(0)))
        );
        assert_eq!(s.log().len(), log);
        assert_eq!(s.disrupted(GroupId(0)), 0);
        // Within budget, but the preemptor still doesn't fit: refused, nothing changes.
        assert_eq!(
            s.apply_preempt(1, &victims[..1], head, &budget).unwrap(),
            PreemptOutcome::Conflict(WorkloadId(2), Conflict::NodeFull)
        );
        s.apply(0, Write::Submit(spec(3, 500))).unwrap();
        assert_eq!(
            s.apply_preempt(2, &victims[..1], (WorkloadId(3), NodeId(0), 1), &budget)
                .unwrap(),
            PreemptOutcome::Applied {
                victims: vec![3],
                placed: 2
            }
        );
        let r = s.record(WorkloadId(0)).unwrap();
        assert_eq!(
            (r.state, r.placements, r.preempted),
            (State::Pending, 1, true)
        );
        assert_eq!(s.disrupted(GroupId(0)), 1);
        let ops: Vec<_> = s.log()[log + 1..].iter().map(|e| (e.write, e.op)).collect();
        let w = ops[0].0;
        assert_eq!(
            ops,
            vec![
                (
                    w,
                    Op::Preempt {
                        workload: WorkloadId(0),
                        node: NodeId(0)
                    }
                ),
                (
                    w,
                    Op::Place {
                        workload: WorkloadId(3),
                        node: NodeId(0)
                    }
                ),
            ]
        );
        s.apply(
            3,
            Write::Complete {
                workload: WorkloadId(3),
                node: NodeId(0),
                version: 2,
            },
        )
        .unwrap();
        s.apply(
            4,
            Write::Place {
                workload: WorkloadId(0),
                node: NodeId(0),
                version: 3,
            },
        )
        .unwrap();
        let r = s.record(WorkloadId(0)).unwrap();
        assert_eq!((r.placements, r.preempted), (2, false));
        assert_eq!(s.disrupted(GroupId(0)), 0);
    }

    #[test]
    fn a_node_is_never_overcommitted() {
        let mut s = store();
        s.apply(0, Write::Submit(spec(1, 700))).unwrap();
        s.apply(0, Write::Submit(spec(2, 700))).unwrap();
        s.apply(
            1,
            Write::Place {
                workload: WorkloadId(1),
                node: NodeId(0),
                version: 1,
            },
        )
        .unwrap();
        let second = s
            .apply(
                2,
                Write::Place {
                    workload: WorkloadId(2),
                    node: NodeId(0),
                    version: 1,
                },
            )
            .unwrap();
        assert_eq!(second, Outcome::Conflict(Conflict::NodeFull));
    }

    /// CLAUDE.md §2: past its bound the store refuses, typed.
    #[test]
    fn records_past_the_bound_are_refused() {
        let mut s = store();
        for id in 0..4 {
            s.apply(0, Write::Submit(spec(id, 1))).unwrap();
        }
        assert!(matches!(
            s.apply(0, Write::Submit(spec(9, 1))),
            Err(SimError::Full { .. })
        ));
    }
}
