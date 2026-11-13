//! The census of inputs and the input-bound rule (the lead, 2026-11-06): a bound on what an input may
//! hold is the most any recorded input holds, times [`INPUT_MARGIN`]. The recorded inputs are the
//! traces committed in bench/sched. A doc-truth test (tests/census.rs) reads them and holds each figure
//! here to theirs, so a trace that holds more names the figure to raise.

/// The input-bound rule's margin (the lead, 2026-11-06).
pub const INPUT_MARGIN: usize = 2;
/// The longest trace, in bytes: `huawei-r5-day30.fleet.keepalive.trace`.
pub const BYTES: usize = 1_772_228;
/// The most node rows: `azure-packing2020-day1.trace`'s 431, the most m5d.metal hosts at which each of
/// its four owners asks for more than a quarter (G4).
pub const NODES: usize = 431;
/// The most legs: the Huawei day's 25,295 launches.
pub const LEGS: usize = 25_295;
/// The most owners: the Huawei day's 339.
pub const OWNERS: usize = 339;
/// The most disruption groups: no committed trace names one.
pub const GROUPS: usize = 0;
/// The most overhead rows: the Huawei traces' 16, OCI's, Firecracker's and Shards' 14 guest sizes.
pub const OVERHEADS: usize = 16;
/// The most Pareto-minimal workload shapes ([`crate::packing::minimal_shapes`]): the Huawei day's 3,
/// one a kind.
pub const SHAPES: usize = 3;
