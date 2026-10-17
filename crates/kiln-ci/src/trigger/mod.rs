//! Events and how workflows' `on:` matches them (docs/ci.md W03).
//!
//! - [`context`]: the `github` context, and payloads for events kiln saw itself.
//! - [`cron`]: schedules, with time zones.
//! - [`event`]: the normalized event.
//! - [`matcher`]: whether a workflow runs for an event, and why not.
//! - [`pattern`]: GitHub's filter patterns.
//! - [`skip`]: skip markers in commit messages.

pub mod context;
pub mod cron;
pub mod event;
pub mod matcher;
pub mod pattern;
pub mod skip;

pub use event::{Event, Source, ZERO_SHA};
pub use matcher::matches;
