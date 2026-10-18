//! CI on the server (docs/ci.md): the integration of `kiln_ci`'s deterministic core with the
//! store, the network and the runners. Each workstream owns its submodules (docs/ci.md §5.2) and
//! adds one line here and one arm in [`api`].

pub mod actions;
pub mod api;
pub mod cli;
pub mod dispatch;
pub mod events;
pub mod facade;
pub mod hooks;
pub mod integrations;
pub mod load;
pub mod oidc;
pub mod rest;
pub mod runs;
pub mod runs_api;
pub mod schedule;
pub mod secrets;
pub mod services;
pub mod token;
pub mod workflows;

/// A failure: an HTTP status and why (as `cd::ops::Fail`).
pub type Fail = (http::StatusCode, String);
