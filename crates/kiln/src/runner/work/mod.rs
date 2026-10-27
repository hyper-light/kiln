//! W24 on the runner (docs/ci.md W24, Design): what a runner keeps so work isn't done twice.
//!
//! - [`store`]: the local content store at `<work>/_cas` (W24-R08).
//! - [`token`]: the cache service's tokens, scoped to a repository, a trust domain and read or
//!   write (W24-R08, R21).
//! - [`service`]: the cache service on loopback and a unix socket, for the runner's legs, its
//!   `kiln run` commands and its agents (W24-R08).
//! - [`upstream`]: where the service's writes go up to and its misses are looked for: the server,
//!   by scope (W24-R08).
//! - [`client`]: a small blocking client of the service.
//! - [`tools`]: the tools' own cache protocols on the service: sccache's WebDAV, Bazel's and
//!   Gradle's and ccache's HTTP caches, Turborepo's API (W24-R08, AC08).
//! - [`gocacheprog`]: `kiln gocacheprog`, Go's `GOCACHEPROG` helper (W24-R08, AC08).
//! - [`peers`]: objects from the runners on this one's LAN, each checked by its digest (W24-R27).

pub mod client;
pub mod gocacheprog;
pub mod peers;
pub mod service;
pub mod store;
pub mod token;
pub mod tools;
pub mod upstream;
