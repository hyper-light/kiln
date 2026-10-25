//! kiln-sim: kiln's deterministic simulator (the critical path's CP-T6.1, docs/sim.md).
//!
//! One seed decides everything a run does, so a failure replays exactly from its seed. Built on
//! kiln-seams' simulators: time is a `SimClock`, tasks run on `SimSpawn`s, and kiln's code under
//! test takes them (and `SimRecords`, `SimHarness` and the rest) through the seams it already
//! uses. What the seams don't cover, kiln-sim simulates itself, under the nemesis:
//!
//! | Part | What it is |
//! |---|---|
//! | [`rng`] | SplitMix64, with named streams |
//! | [`world`] | the seeded scheduler: nodes' tasks polled one at a time in the seed's order, crashes that drop them at any await |
//! | [`queue`] | a model's own events, by when they're due |
//! | [`net`] | frames between nodes while the nemesis lasts: datagrams dropped, duplicated and overtaken, frames on an ordered lane dropped but never reordered, requests dropped or late, links partitioned |
//! | [`disk`] | a machine's disk: what's durable apart from what's seen, power losses that tear, lose or keep what wasn't fsynced, fsync errors, ENOSPC, a crash at any write |
//! | [`records`] | a crash at any commit of a kiln-seams `Records` store, before it takes effect or after |
//! | [`trace`] | what a run did, folded into a digest (and its last steps in words, on a replay) |
//! | [`seeds`] | budgets, seeds a minute, replay of one seed, and running a seed twice |
//!
//! The models that run kiln's code under it (its sync engine, session moves and goal driver) live
//! beside that code in kiln (`crates/kiln/src/sim`, `goals/sim.rs`), and run in kiln's CI on every
//! change request (a budget of seeds) and nightly (a large one). docs/sim.md says what they run,
//! what they check, the bugs they found, and what isn't modelled yet.
//!
//! The bar is kiln's: no panics (the workspace lints), no unchecked arithmetic outside tests,
//! every queue bounded with its overflow stated.

#![cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]

pub mod disk;
pub mod net;
pub mod queue;
pub mod records;
pub mod rng;
pub mod seeds;
pub mod trace;
pub mod world;

pub use disk::{Disk, DiskFaults};
pub use net::{Faults, Net, Node};
pub use queue::Queue;
pub use rng::Rng;
pub use trace::Trace;
pub use world::World;
