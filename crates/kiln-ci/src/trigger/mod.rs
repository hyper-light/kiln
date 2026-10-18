//! Events and how workflows' `on:` matches them (docs/ci.md W03).
//!
//! - [`context`]: the `github` context, and payloads for events kiln saw itself.
//! - [`cron`]: schedules, with time zones.
//! - [`dispatch`]: `workflow_dispatch` inputs, typed and checked.
//! - [`event`]: the normalized event.
//! - [`github`]: GitHub's webhook deliveries as events.
//! - [`matcher`]: whether a workflow runs for an event, and why not.
//! - [`pattern`]: GitHub's filter patterns.
//! - [`skip`]: skip markers in commit messages.

pub mod context;
pub mod cron;
pub mod dispatch;
pub mod event;
pub mod github;
pub mod matcher;
pub mod pattern;
pub mod skip;

pub use event::{Event, Source, ZERO_SHA};
pub use matcher::matches;
