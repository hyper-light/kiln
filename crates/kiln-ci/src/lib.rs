//! kiln's CI core (docs/ci.md): everything that *decides*, written as pure code with no clock,
//! no disk, no network and no randomness except what the caller passes in, the way
//! `kiln-cd`'s engine is. The kiln crate feeds it events and carries out what it says, so
//! every decision here is testable at full speed and benchmarkable.
//!
//! - [`expr`]: the `${{ }}` expression language, GitHub-exact, with kiln's additions (W01).
//! - [`wf`]: workflows of both dialects, overlays and action metadata, validated (W02).
//! - [`trigger`]: events and how workflows match them (W03).
//! - [`plan`]: a run's graph, matrices, reusable workflows, outputs (W04).
//! - [`policy`]: which remote actions a run may use (W07).
//! - [`oidc`]: the OIDC issuer's key rotation, claims and subjects (W10).
//! - [`sched`]: queues, concurrency, priorities, fairness, routing, retries (W05).
//! - [`results`]: test reports and flaky classification (W16).
//! - [`runner`]: what the runner decides: workflow commands, environment files, masking (W06).
//! - [`store`]: chunking and GitHub's cache rules for the runtime store (W08).
//! - [`secrets`]: secret scopes, and which secrets a leg reads (W09).
//! - [`stats`]: bootstrap intervals for benchmarks and experiments (W16, W20).
//! - [`facts`]: what runners report about themselves (W13).
//! - [`wire`]: the JSON payloads between the server and CI runners (§2.5).

pub mod env;
pub mod expr;
pub mod facts;
pub mod oidc;
pub mod plan;
pub mod policy;
pub mod results;
pub mod runner;
pub mod sched;
pub mod secrets;
pub mod stats;
pub mod store;
pub mod trigger;
pub mod wf;
pub mod wire;
