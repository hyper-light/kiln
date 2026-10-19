//! Scheduling: queues, concurrency, priorities, fairness, routing, retries (docs/ci.md W05).
//!
//! - [`state`]: the scheduler.
//! - [`sim`]: a simulator driving it.
//! - [`groups`]: concurrency groups.
//! - [`retry`]: retry policies and failure classes.
//! - [`routing`]: requirements, rules, eligibility, preferences, explanations.
//! - [`pools`]: pools of executors kiln starts and stops (W13-R09).

pub mod groups;
pub mod pools;
pub mod retry;
pub mod routing;
pub mod sim;
pub mod state;

#[cfg(test)]
mod tests;

pub use state::{Command, Config, Input, LegIn, Outcome, Resources, RunnerIn, Scheduler, Supersede, WaitReason};
