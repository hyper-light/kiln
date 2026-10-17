//! CI on the server (docs/ci.md): the integration of `kiln_ci`'s deterministic core with the
//! store, the network and the runners. Each workstream owns its submodules (docs/ci.md §5.2) and
//! adds one line here and one arm in [`api`].

pub mod api;
pub mod load;
pub mod workflows;

/// A failure: an HTTP status and why (as `cd::ops::Fail`).
pub type Fail = (http::StatusCode, String);
