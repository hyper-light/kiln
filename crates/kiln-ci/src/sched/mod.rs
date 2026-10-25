//! Scheduling: queues, concurrency, priorities, fairness, routing, retries (docs/ci.md W05).
//!
//! - [`state`]: the scheduler.
//! - [`sim`]: a simulator driving it.
//! - [`groups`]: concurrency groups.
//! - [`retry`]: retry policies and failure classes.
//! - [`routing`]: requirements, rules, eligibility, preferences, explanations.
//! - [`pools`]: pools of executors kiln starts and stops (W13-R09).
//! - [`reserve`]: runners and pools kept for some legs in windows (W05-R12).
//! - [`admit`]: heavy legs held off machines under I/O pressure or short of disk (W24-R19).
//! - [`warm`]: where a leg is expected to finish first from runners' warm state (W24-R18).

pub mod admit;
pub mod groups;
pub mod pools;
pub mod reserve;
pub mod retry;
pub mod routing;
pub mod sim;
pub mod state;
pub mod warm;

#[cfg(test)]
mod tests;

pub use state::{Command, Config, Input, LegIn, Outcome, Resources, RunnerIn, Scheduler, Supersede, WaitReason};
