//! Where a dangerous workload may run again (S8; F7 in `docs/design/failure-isolation.md`, `hex-sim.md`
//! H42): by the kinds Hex places, G2's [`Class`]. A workload that trips a security signal
//! ([`hex_health::danger::trips`]) is stopped at once, its node's share given back; that is its
//! isolation. Then the scheduler re-places it, in a stronger kind, anywhere in the cell, or refuses it,
//! typed.
//!
//! - An OCI container shares its host's kernel; a Firecracker or Shards microVM runs its own under a
//!   hypervisor (Agache et al., NSDI '20, FI[19]). So a tripped OCI container is re-placed as a
//!   microVM: the first of [`STRONGER`] that some node of the cell admits, by G2's own fast path
//!   ([`FitIndex::fit`]) and its filters, the class filter among them, with its need plus that
//!   kind's overhead. Nothing ties it to the node it tripped on: it starts again either way. A GPU
//!   workload fits no microVM: neither takes one ([`Class::takes_gpus`]).
//! - Firecracker before Shards: Firecracker's isolation is published and measured (a minimal device
//!   model, NSDI '20, `docs/research/runtime/substrates-and-sandboxes.md` [SUB-11]); Shards' device
//!   model isn't public in the sources read (`docs/research/scheduler/agentic-workloads.md`, the
//!   device table). Their overheads don't decide it: Shards' is a 5 MiB stand-in for Firecracker's.
//! - A trip inside a microVM is refused: no kind Hex places isolates more strongly.

use hex_health::danger::{self, Signal};

use crate::SimError;
use crate::index::{FitIndex, Pick, Verdict};
use crate::model::{Class, Demand, Kind, NodeId, Overheads, Resources, WorkloadId};

/// The kinds a tripped OCI container may be re-placed as, in the order it tries them.
pub const STRONGER: [Class; 2] = [Class::Firecracker, Class::Shards];

/// A placed workload that raised a signal: where, its own need (its kind's overhead not counted), its
/// kind of work and the kind it runs as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tripped {
    pub id: WorkloadId,
    pub node: NodeId,
    pub need: Resources,
    pub kind: Kind,
    pub class: Class,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Isolation {
    /// Not a trip: nothing to do.
    None,
    /// Stopped, and re-placed as `to` on `node`; `scanned` the nodes the pick read.
    Move {
        from: Class,
        to: Class,
        node: NodeId,
        scanned: usize,
    },
    /// Stopped, and started nowhere.
    Refuse { from: Class, why: Refusal },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// It tripped in a microVM: no kind isolates more strongly.
    InMicroVm,
    /// No node of the cell runs a microVM.
    NoMicroVmInCell,
    /// Nodes run a microVM, and none admits it: too little room, or GPUs no microVM takes.
    NoneHolds,
}

/// `need` with `class`'s overhead for its memory counted, as G2 counts it in a workload's need; refused
/// where `class`'s overhead goes by guest size and names no row for it.
fn with_overhead(
    need: &Resources,
    class: Class,
    overheads: &Overheads,
) -> Result<Resources, SimError> {
    let mem_bytes = overheads.of(class, need.mem_bytes).ok_or(SimError::Config(
        "the class's overhead names no row for the workload's memory",
    ))?;
    need.checked_add(&Resources {
        mem_bytes,
        ..Resources::default()
    })
    .ok_or(SimError::Config("a need past u64"))
}

/// Isolate `tripped`, which raised `signal`, in the cell `index` holds: stop it (its share given back
/// to its node) and re-place it by `sample`, taking its new place in `index`.
pub fn isolate(
    tripped: &Tripped,
    signal: Signal,
    index: &mut FitIndex,
    overheads: &Overheads,
    pick: &Pick,
) -> Result<Isolation, SimError> {
    if !danger::trips(signal) {
        return Ok(Isolation::None);
    }
    let from = tripped.class;
    let running = Demand {
        need: with_overhead(&tripped.need, from, overheads)?,
        kind: tripped.kind,
        class: from,
        ask: Default::default(),
    };
    index.give(tripped.node, tripped.id, &running)?;
    if from != Class::Oci {
        return Ok(Isolation::Refuse {
            from,
            why: Refusal::InMicroVm,
        });
    }
    let mut runs_one = false;
    for to in STRONGER {
        let demand = Demand {
            need: with_overhead(&tripped.need, to, overheads)?,
            kind: tripped.kind,
            class: to,
            ask: Default::default(),
        };
        let fit = index.fit(&demand, pick);
        if let Some(key) = fit.best {
            index.take(key.node, tripped.id, &demand)?;
            return Ok(Isolation::Move {
                from,
                to,
                node: key.node,
                scanned: usize::try_from(fit.walked).unwrap_or(usize::MAX),
            });
        }
        // None holds it: some node runs this kind, or none does.
        runs_one |= (0..index.len())
            .filter_map(|at| u32::try_from(at).ok())
            .any(|at| index.runs(NodeId(at), to));
    }
    Ok(Isolation::Refuse {
        from,
        why: if runs_one {
            Refusal::NoneHolds
        } else {
            Refusal::NoMicroVmInCell
        },
    })
}

/// Whether some node of `index` admits `tripped`'s need as `to` now (an oracle's question).
pub fn admitted_as(
    index: &FitIndex,
    tripped: &Tripped,
    to: Class,
    overheads: &Overheads,
) -> Result<bool, SimError> {
    let demand = Demand {
        need: with_overhead(&tripped.need, to, overheads)?,
        kind: tripped.kind,
        class: to,
        ask: Default::default(),
    };
    Ok(index.admitted_anywhere(&demand))
}

/// Whether some node of `index` runs `to` (an oracle's question).
pub fn runs(index: &FitIndex, to: Class) -> Result<bool, SimError> {
    let probe = Demand {
        need: Resources::default(),
        kind: Kind::Plain,
        class: to,
        ask: Default::default(),
    };
    for at in 0..index.len() {
        let id = u32::try_from(at).map_err(|_| SimError::Config("a node id past u32"))?;
        if index.verdict(NodeId(id), &probe)? != Verdict::NoClass {
            return Ok(true);
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Classes, NodeSpec};

    const MIB: u64 = 1 << 20;

    fn overheads() -> Overheads {
        let mut o = Overheads::default();
        o.set(Class::Oci, 440_402);
        o.set(Class::Firecracker, 5 * MIB);
        o.set(Class::Shards, 5 * MIB);
        o
    }

    fn res(cpu_milli: u64, mem_mib: u64, gpus: u32) -> Resources {
        Resources {
            cpu_milli,
            mem_bytes: mem_mib * MIB,
            gpus,
            disk_bytes: 0,
        }
    }

    fn only(classes: &[Class]) -> Classes {
        classes.iter().fold(Classes::NONE, |c, k| c.with(*k))
    }

    /// A cell of nodes with these classes and capacities; the tripped OCI container on node 0, placed.
    fn cell(nodes: &[(Classes, Resources)], need: Resources) -> (FitIndex, Tripped) {
        let specs: Vec<NodeSpec> = nodes
            .iter()
            .enumerate()
            .map(|(i, (classes, capacity))| NodeSpec {
                routes: None,
                costs: [None; crate::model::Class::COUNT],
                priors: Default::default(),
                runtime_pools: Vec::new(),
                id: NodeId(u32::try_from(i).unwrap()),
                capacity: *capacity,
                background_milli: 0,
                classes: *classes,
                platform: Default::default(),
                virt: Default::default(),
                // A node's GPUs are listed as its capacity counts them (NodeSpec::devices_agree).
                devices: crate::model::unnamed_gpus(capacity.gpus),
            })
            .collect();
        let mut index = FitIndex::new(&specs).unwrap();
        let t = Tripped {
            id: WorkloadId(0),
            node: NodeId(0),
            need,
            kind: Kind::Plain,
            class: Class::Oci,
        };
        let running = Demand {
            need: with_overhead(&need, Class::Oci, &overheads()).unwrap(),
            kind: Kind::Plain,
            class: Class::Oci,
            ask: Default::default(),
        };
        index.take(NodeId(0), WorkloadId(0), &running).unwrap();
        (index, t)
    }

    /// S8 dangerous (F7): a tripped OCI container whose own node runs no microVM is re-placed on
    /// another node of the cell that runs Firecracker; where only Shards is run, as Shards.
    #[test]
    fn an_oci_trip_is_re_placed_anywhere_in_the_cell_as_the_first_microvm_that_holds_it() {
        let need = res(500, 512, 0);
        for s in [
            Signal::SeccompKill,
            Signal::EgressRefused,
            Signal::PrivilegeEscalation,
        ] {
            let (mut index, t) = cell(
                &[
                    (only(&[Class::Oci]), res(4000, 4096, 0)),
                    (only(&[Class::Shards]), res(4000, 4096, 0)),
                    (only(&[Class::Firecracker]), res(4000, 4096, 0)),
                ],
                need,
            );
            let iso = isolate(&t, s, &mut index, &overheads(), &Pick::default()).unwrap();
            assert!(
                matches!(
                    iso,
                    Isolation::Move {
                        from: Class::Oci,
                        to: Class::Firecracker,
                        node: NodeId(2),
                        ..
                    }
                ),
                "{iso:?}"
            );
            let (mut index, t) = cell(
                &[
                    (only(&[Class::Oci]), res(4000, 4096, 0)),
                    (only(&[Class::Shards]), res(4000, 4096, 0)),
                ],
                need,
            );
            let iso = isolate(&t, s, &mut index, &overheads(), &Pick::default()).unwrap();
            assert!(
                matches!(
                    iso,
                    Isolation::Move {
                        to: Class::Shards,
                        node: NodeId(1),
                        ..
                    }
                ),
                "{iso:?}"
            );
        }
    }

    /// Re-placed on its own node, the move counts the container's share given back and the
    /// microVM's overhead: with exactly room for both it fits, a byte less and it doesn't.
    #[test]
    fn the_move_counts_both_overheads() {
        let need = res(500, 512, 0);
        // Capacity: the container's share plus 5 MiB less its 0.42 MiB overhead.
        let exact = need.mem_bytes + 5 * MIB;
        for (mem, fits) in [(exact, true), (exact - 1, false)] {
            let capacity = Resources {
                cpu_milli: 500,
                mem_bytes: mem,
                gpus: 0,
                disk_bytes: 0,
            };
            let (mut index, t) = cell(&[(Classes::ALL, capacity)], need);
            let iso = isolate(
                &t,
                Signal::SeccompKill,
                &mut index,
                &overheads(),
                &Pick::default(),
            )
            .unwrap();
            if fits {
                assert!(
                    matches!(
                        iso,
                        Isolation::Move {
                            to: Class::Firecracker,
                            node: NodeId(0),
                            ..
                        }
                    ),
                    "{iso:?}"
                );
            } else {
                assert_eq!(
                    iso,
                    Isolation::Refuse {
                        from: Class::Oci,
                        why: Refusal::NoneHolds
                    }
                );
            }
        }
    }

    /// Refusals, typed: a trip in either microVM; a cell running no microVM; a GPU workload, which
    /// no microVM takes. A refused container stays stopped: its share is given back either way.
    #[test]
    fn a_trip_with_no_microvm_in_the_cell_that_holds_it_is_refused() {
        let need = res(500, 512, 0);
        for from in [Class::Firecracker, Class::Shards] {
            let (mut index, mut t) = cell(&[(Classes::ALL, res(4000, 4096, 0))], need);
            index
                .give(
                    NodeId(0),
                    WorkloadId(0),
                    &Demand {
                        need: with_overhead(&need, Class::Oci, &overheads()).unwrap(),
                        kind: Kind::Plain,
                        class: Class::Oci,
                        ask: Default::default(),
                    },
                )
                .unwrap();
            let running = Demand {
                need: with_overhead(&need, from, &overheads()).unwrap(),
                kind: Kind::Plain,
                class: from,
                ask: Default::default(),
            };
            index.take(NodeId(0), WorkloadId(0), &running).unwrap();
            t.class = from;
            assert_eq!(
                isolate(
                    &t,
                    Signal::SeccompKill,
                    &mut index,
                    &overheads(),
                    &Pick::default()
                )
                .unwrap(),
                Isolation::Refuse {
                    from,
                    why: Refusal::InMicroVm
                }
            );
            assert_eq!(index.key(NodeId(0)).unwrap().mem_bytes, 4096 * MIB);
        }
        let (mut index, t) = cell(
            &[
                (only(&[Class::Oci]), res(4000, 4096, 0)),
                (only(&[Class::Oci]), res(4000, 4096, 0)),
            ],
            need,
        );
        assert_eq!(
            isolate(
                &t,
                Signal::EgressRefused,
                &mut index,
                &overheads(),
                &Pick::default()
            )
            .unwrap(),
            Isolation::Refuse {
                from: Class::Oci,
                why: Refusal::NoMicroVmInCell
            }
        );
        let gpu = res(500, 512, 1);
        let (mut index, t) = cell(&[(Classes::ALL, res(4000, 4096, 1))], gpu);
        assert_eq!(
            isolate(
                &t,
                Signal::PrivilegeEscalation,
                &mut index,
                &overheads(),
                &Pick::default()
            )
            .unwrap(),
            Isolation::Refuse {
                from: Class::Oci,
                why: Refusal::NoneHolds
            }
        );
    }

    /// An errno'd call is not a trip, in any kind: nothing is stopped.
    #[test]
    fn an_errno_is_not_a_trip() {
        let need = res(500, 512, 0);
        let (mut index, t) = cell(&[(Classes::ALL, res(4000, 4096, 0))], need);
        let before = index.key(NodeId(0)).unwrap();
        assert_eq!(
            isolate(
                &t,
                Signal::SeccompErrno,
                &mut index,
                &overheads(),
                &Pick::default()
            )
            .unwrap(),
            Isolation::None
        );
        assert_eq!(index.key(NodeId(0)).unwrap(), before);
    }
}
