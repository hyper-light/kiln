//! CI on the server (docs/ci.md): the integration of `kiln_ci`'s deterministic core with the
//! store, the network and the runners. Each workstream owns its submodules (docs/ci.md §5.2) and
//! adds one line here and one arm in [`api`].

pub mod actions;
pub mod api;
pub mod attest;
pub mod bench;
pub mod cd_events;
pub mod broker;
pub mod cli;
pub mod compare;
pub mod cr;
pub mod deploy_step;
pub mod diffs;
pub mod dispatch;
pub mod environments;
pub mod events;
pub mod facade;
pub mod hooks;
pub mod hosting;
pub mod inbound;
pub mod images;
pub mod inbox;
pub mod pools;
pub mod insights;
pub mod land;
pub mod integrations;
pub mod load;
pub mod local;
pub mod logsearch;
pub mod notify;
pub mod oidc;
pub mod ops;
pub mod preview;
pub mod providers;
pub mod registry;
pub mod signing;
pub mod smtp;
pub mod repo_config;
pub mod results;
pub mod rest;
pub mod reusable;
pub mod review;
pub mod review_cli;
pub mod reuse;
pub mod routing;
pub mod runs;
pub mod runs_api;
pub mod sbom;
pub mod schedule;
pub mod secrets;
pub mod services;
pub mod token;
pub mod uploads;
pub mod webhooks;
pub mod workflows;

/// A failure: an HTTP status and why (as `cd::ops::Fail`).
pub type Fail = (http::StatusCode, String);
