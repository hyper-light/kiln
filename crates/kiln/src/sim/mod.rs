//! kiln-sim's models of kiln (CP-T6.1, docs/sim.md): each runs part of kiln's own code under
//! kiln-sim's seeded scheduler, network and disks, with the nemesis, and checks what came out. A
//! failure names its seed; `KILN_SIM_SEED=<seed>` replays that seed alone, with its last steps.
//!
//! | Model | What of kiln's it runs |
//! |---|---|
//! | [`sync`] | a synced repo's copies and the server's member: `sync::Copy`, `Feed`, `Index`, `take_steps`, hyper-durable replicas, the runner's and server's shells |
//! | [`moves`] | the same, with an agent session moving between the machines: `sync::runner`'s `look_now_at`, `left_at` and `arrived`, as `harness::session` moves one |
//! | [`transfers`] | agent sessions changing hands between machines' runner processes and kiln's server (moves, resumes from the server's store, take-overs, blue/green upgrades and rollbacks), with copies left behind still acting: the server's act check, its signed words and the runner's check of them, the custody chain's hop and walk (W27-AC05) |
//! | [`sched`] | the CI engine (`kiln_ci::sched::Scheduler`) on the server beside its store, runners taking or refusing its grants by their own admission (`agent::admits`) and keeping their leases by their own keeper (`runner::lease::Leases`), as the dispatcher and hub carry them, through the server's restarts |
//! | `goals::sim` | the goal driver: `goals`' `adopt` and `step` on the seeded scheduler (beside the driver, whose private parts it runs) |

pub mod fs;
pub mod moves;
pub mod sched;
pub mod sync;
pub mod transfers;
