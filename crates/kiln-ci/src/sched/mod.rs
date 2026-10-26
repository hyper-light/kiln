//! Scheduling: queues, concurrency, priorities, fairness, routing, retries (docs/ci.md W05).
//!
//! - [`state`]: the scheduler.
//! - [`sim`]: a simulator driving it.
//! - [`groups`]: concurrency groups.
//! - [`demand`]: demands, the work in flight, and what becomes of work no one wants (W28-R40 to R44).
//! - [`retry`]: retry policies and failure classes.
//! - [`routing`]: requirements, rules, eligibility, preferences, explanations.
//! - [`pools`]: pools of executors kiln starts and stops (W13-R09).
//! - [`reserve`]: runners and pools kept for some legs in windows (W05-R12).

pub mod demand;
pub mod groups;
pub mod pools;
pub mod reserve;
pub mod retry;
pub mod routing;
pub mod sim;
pub mod state;

#[cfg(test)]
mod tests;

pub use state::{Command, Config, Input, LegIn, Outcome, Resources, RunnerIn, Scheduler, Supersede, WaitReason};
