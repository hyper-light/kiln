//! Scheduling: queues, concurrency, priorities, fairness, routing, retries (docs/ci.md W05).
//!
//! - [`state`]: the scheduler.
//! - [`sim`]: a simulator driving it.
//! - [`actor`]: whose a commit is, from kiln's records (W28-R67).
//! - [`admit`]: admission: coalescing, cadence, speculation windows (W28-R60 to R63, R68).
//! - [`demand`]: demands, the work in flight, and what becomes of work no one wants (W28-R40 to R44).
//! - [`groups`]: concurrency groups.
//! - [`intent`]: priority by intent: gate, waiting, trunk, work in progress (W28-R64, R65).
//! - [`needs`]: what work needs of a runner, in one grammar (docs/runners.md).
//! - [`retry`]: retry policies and failure classes.
//! - [`routing`]: requirements, rules, eligibility, preferences, explanations.
//! - [`pools`]: pools of executors kiln starts and stops (W13-R09).
//! - [`reserve`]: runners and pools kept for some legs in windows (W05-R12).
//! - [`load`]: heavy legs held off machines under I/O pressure or short of disk (W24-R19).
//! - [`warm`]: where a leg is expected to finish first from runners' warm state (W24-R18).

pub mod actor;
pub mod admit;
pub mod demand;
pub mod load;
pub mod groups;
pub mod intent;
pub mod needs;
pub mod pools;
pub mod reserve;
pub mod retry;
pub mod routing;
pub mod sim;
pub mod state;
pub mod warm;

#[cfg(test)]
mod tests;

pub use state::{Command, Config, FairBy, Input, LegIn, Outcome, PinnedState, Placement, Resources, RunnerIn, Scheduler, Supersede, WaitReason};
