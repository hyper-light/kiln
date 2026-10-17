//! kiln's deployment engine: what it takes to render, compare and sync applications, written
//! as a state machine with no clock, no disk and no network, the way hyper-raft's core is.
//!
//! The engine ([`engine`]) is told what happened (an app's render inputs moved, a render
//! finished, a live object changed, an apply answered, time passed) and says what to do
//! (render this, watch that kind, apply this object, delete that one, record this deployment).
//! Its owner, the kiln server, does the I/O. Because nothing here waits on anything, the
//! benchmarks drive the same engine production runs, against simulated clusters, at full speed.
//!
//! - [`apps`]: app definitions from `.kiln/deploy.yaml`, Argo CD Applications and the API.
//! - [`git`]: mirrors, revisions, content hashes of paths, extraction (the `git` binary).
//! - [`render`]: directories, Helm, Kustomize and plugins, with what each render reads.
//! - [`kube`]: kubeconfig, credential plugins, discovery, request paths, watch streams.
//! - [`obj`]: object identity, parsing, structural hashes, compressed storage.
//! - [`model`]: app configuration, sync options and policies (Argo CD's spellings).
//! - [`health`]: resource health, rule for rule as Argo CD assesses it.
//! - [`diff`]: comparing desired and live objects by field ownership.
//! - [`sync`]: planning a sync: phases, waves, hooks and prunes, in Argo CD's order.
//! - [`engine`]: the controller.
//! - [`tree`]: an app's resource tree: its objects and the live objects they own.
//! - [`state`] and [`store`]: what is kept durably, replicated by hyper-raft on hyper-log.
//! - [`sim`] and [`harness`]: simulated clusters, and the engine driven against them on a
//!   simulated clock (tests and `kiln cd bench`).

pub mod apps;
pub mod bench;
pub mod diff;
pub mod engine;
pub mod git;
pub mod harness;
pub mod health;
pub mod kube;
pub mod model;
pub mod obj;
pub mod render;
pub mod sim;
pub mod state;
pub mod store;
pub mod sync;
pub mod tree;

#[cfg(test)]
mod tests;
