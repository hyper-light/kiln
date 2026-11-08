//! Service-aware protection (LB05, docs/protect.md): what kiln protects on a machine, how fast it
//! must act to protect it, how it tells harm from noise, what evidence a cause needs before kiln acts
//! on it, and the guard that stops kiln's heaviest work (the gate, `ci/gate.sh`) when the machine's
//! own services suffer. Pure code (docs/ci.md §0.3): the caller passes in what it measured and
//! when, and carries out what comes back.
//!
//! - [`objective`]: the requests someone waits on, by who waits; their deadlines; and how a request
//!   that failed fast counts (as failed service, never as a fast answer);
//! - [`rto`]: RFC 6298's estimator, for any deadline on an answer that may come late;
//! - [`budget`]: the reaction budget (sensing, deciding, acting, and draining what was already
//!   issued), and the PSI windows the kernel lets a watcher use;
//! - [`stall`]: a CUSUM on a stall the kernel counts, exact though only the kernel wakes it;
//! - [`cause`]: which resource a slowdown can be laid on, from what was measured, and when what the
//!   disks delivered says what they take;
//! - [`guard`]: the gate's guard, a state machine of events in and decisions out, each explained and
//!   replayable from its inputs.
//!
//! No number here is picked: each is measured, derived from a cited source, the platform's own, or a
//! person's setting whose default says where it comes from.

pub mod budget;
pub mod cause;
pub mod guard;
pub mod objective;
pub mod rto;
pub mod stall;
