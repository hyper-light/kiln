//! hex-sim: the simulated cluster the scheduler's bars are measured in (`docs/design/scheduler-architecture.md`,
//! "hex-sim: the measurement vehicle"; `docs/design/hex-sim.md`). A pure core (CLAUDE.md §9): one
//! seed, virtual time and a single-threaded event loop decide everything, so a run is reproducible
//! and a regression is a changed number, not noise.
//!
//! - [`capacity`]: one machine's capacity view under a nemesis: Hex's own boxes and kiln's runners on its
//!   keeper, crashes on either side, dropped links, partitions and heals (joint plan step 13).
//! - [`census`]: the most the recorded inputs hold, and the input-bound rule that bounds inputs by it.
//! - [`charge`]: a microVM's charge learned from its measured use, its bookings within its node's fleet budget,
//!   and the contention rule's decisions (joint plan step 14, `docs/design/learned-charge.md`).
//! - [`model`]: nodes, workloads and their resources; nodes' platforms and devices, workloads' asks.
//! - [`needs`]: what a box needs of a node, in kiln's needs grammar adapted to boxes.
//! - [`facts`]: what a node reports about itself, read from its OS's text, and the node it makes.
//! - [`index`]: the best-fit index both the scheduler and its serial specification place with.
//! - [`store`]: the control plane's durable records, committed by compare-and-set (S6).
//! - [`sim`]: the cluster: clients, a control plane of parallel deciders and an in-order validating
//!   committer, fake node agents, and the nemesis that crashes the control plane at a chosen write.
//! - [`spec`]: the serial specification (one scheduler, one cycle) as the oracle a run's log is
//!   checked against (`T-T7.4.1`), and the end-state check that nothing was lost or started twice
//!   (`AC-T7.3.3`).
//! - [`fair`]: the order between owners' queues (bands, weighted DRF) and the shares a run gave them.
//! - [`generate`]: generated clusters and workloads from a seed.
//! - [`replay`]: a recorded workload read from text, for replaying a real day.
//! - [`packing`]: stranded capacity at a load, the measure of `AC-T7.4.3`.
//! - [`danger`]: where a workload that trips a security signal may run again, by the kinds Hex places
//!   (S8).
//! - [`faults`]: the fault nemesis, each fault class against its detection and isolation bound (S8).
//! - [`membership`]: SWIM with Lifeguard on memberlist's protocol, the competitor: crash detection and
//!   slow-member false positives (S8).
//! - [`swim`]: hyper-swim's own failure detector, the membership the design gives the node agent, vendored and
//!   driven over a simulated LAN: the crash class's bound (S8, F1).
//! - [`federation`]: cells and regions, a federation over independent cells (H64-H66).
//! - [`peer`]: placement with no plane, each node owning the boxes asked of it and admitting the boxes
//!   it runs, under kiln's restart rules: p2p, and a laptop alone.
//! - [`lb`]: the placement kernel, ported from kiln's load balancer and adapted to boxes: an owner's
//!   pick that packs, a node's ledger that admits, and the words between them.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]

pub mod baseline;
pub mod capacity;
pub mod census;
pub mod charge;
pub mod danger;
pub mod error;
pub mod facts;
pub mod fair;
pub mod faults;
pub mod federation;
pub mod generate;
pub mod index;
pub mod lb;
pub mod membership;
pub mod model;
pub mod needs;
pub mod packing;
pub mod peer;
pub mod proximity;
pub mod replay;
pub mod rng;
pub mod sim;
pub mod spec;
pub mod start_cost;
pub mod store;
pub mod swim;
pub mod trace;
/// hyper-swim's Vivaldi engine, vendored unchanged from hyper-raft (`vendor/MANIFEST.toml`,
/// `docs/design/hex-sim.md` H20): `scripts/vendor.py check` proves it is upstream's, byte for byte.
#[path = "../../../../hyper-raft/crates/hyper-swim/src/coordinates.rs"]
pub mod vivaldi;
pub mod wan;

pub use error::SimError;
