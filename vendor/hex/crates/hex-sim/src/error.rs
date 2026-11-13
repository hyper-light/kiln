//! Every way a simulation refuses to go on, typed (CLAUDE.md §3).

use crate::model::{NodeId, WorkloadId};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum SimError {
    #[error("the configuration is refused: {0}")]
    Config(&'static str),
    #[error("{what} is full at its bound of {bound}")]
    Full { what: &'static str, bound: usize },
    #[error("the run took more than its budget of {0} events")]
    StepBudget(u64),
    #[error("virtual time overflowed")]
    TimeOverflow,
    #[error("a resource count overflowed or went below zero on node {0:?}")]
    Resources(NodeId),
    #[error("node {0:?} is not in the cluster")]
    UnknownNode(NodeId),
    #[error("workload {0:?} is not in the run")]
    UnknownWorkload(WorkloadId),
    #[error(
        "workload {0:?}'s memory can't move to its new class: an overflow, or no overhead for its size"
    )]
    Need(WorkloadId),
    #[error("workload {0:?}'s client gave up after {1} attempts")]
    ClientGaveUp(WorkloadId, u32),
    #[error("node {0:?} gave up reporting workload {1:?} done after {2} attempts")]
    ReportGaveUp(NodeId, WorkloadId, u32),
    #[error(
        "the control plane's own commit of {0:?} conflicted, which its sole-writer design rules out"
    )]
    UnexpectedConflict(WorkloadId),
}
