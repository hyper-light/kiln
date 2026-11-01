//! kiln-sim's models of kiln (CP-T6.1, docs/sim.md): each runs part of kiln's own code under
//! kiln-sim's seeded scheduler, network and disks, with the nemesis, and checks what came out. A
//! failure names its seed; `KILN_SIM_SEED=<seed>` replays that seed alone, with its last steps.
//!
//! | Model | What of kiln's it runs |
//! |---|---|
//! | [`sync`] | a synced repo's copies and the server's member: `sync::Copy`, `Feed`, `Index`, `take_steps`, hyper-durable replicas, the runner's and server's shells |
//! | [`moves`] | the same, with an agent session moving between the machines: `sync::runner`'s `look_now_at`, `left_at` and `arrived`, as `harness::session` moves one |
//! | [`transfers`] | agent sessions changing hands between machines' runner processes and kiln's server (moves, resumes from the server's store, take-overs, blue/green upgrades and rollbacks), with copies left behind still acting: the server's act check, its signed words and the runner's check of them, the custody chain's hop and walk (W27-AC05) |
//! | [`custody`] | things handed on between custodians, directly and through relays that drop, change, swap or replay what they carry and lose their own records: each taker's check of what came against its giver's hand-on, and kiln-ci's custody hop, walk and gaps as kiln's server walks a chain (W27-R08) |
//! | `goals::sim` | the goal driver: `goals`' `adopt` and `step` on the seeded scheduler (beside the driver, whose private parts it runs) |

pub mod custody;
pub mod fs;
pub mod moves;
pub mod sync;
pub mod transfers;
