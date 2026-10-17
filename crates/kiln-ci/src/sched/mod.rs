//! Scheduling: queues, concurrency, priorities, fairness, routing, retries (docs/ci.md W05).
//!
//! - [`groups`]: concurrency groups.
//! - [`retry`]: retry policies and failure classes.
//! - [`routing`]: requirements, rules, eligibility, preferences, explanations.

pub mod groups;
pub mod retry;
pub mod routing;
