//! Doing the least work a change needs (docs/ci.md W24): every unit of work keyed by what it
//! depends on, so a recorded success can be reused and a rebuild can say why.
//!
//! - [`key`]: weak and strong fingerprints (W24-R01, R03 to R05).
//! - [`graph`]: package graphs from manifests, and the packages a change affects (W24-R16, R17).
//! - [`ops`]: container operations as a DAG of W24 units, and the Dockerfile frontend that lowers
//!   a Dockerfile to them (W24-R29).
//! - [`slots`]: warm workspaces, decided: lanes, leases, clones, budgets (W24-R11, R15).
//! - [`tests`]: test binaries' results as units of work, and which run or replay (W24-R10).
//! - [`queue`]: what a land candidate and its bisection reuse (W24-R20).
//! - [`evict`]: what a domain keeps under quota pressure (W24-R15, R21, AC12).
//! - [`eclass`]: equivalence classes over keys, as an e-graph's: union-find, congruence, rewrite
//!   rules with witnesses, splits (W24-R30).
//! - [`why`]: what each unit did, and why one ran: fingerprints compared (W24-R28).
//! - [`memo`]: a step's `memo:`, its key, what a success leaves and its files as trees (W24-R07).
//! - [`pathset`]: path sets from the tools' own records (W24-R04, R05).
//! - [`trust`]: trust domains, lineage, admission and quotas (W24-R21, R22).
//! - [`seal`]: what runners sign of a result and the server countersigns, and what readers make
//!   of it (W24-R22).

pub mod eclass;
pub mod evict;
pub mod graph;
pub mod key;
pub mod memo;
pub mod ops;
pub mod pathset;
pub mod queue;
pub mod seal;
pub mod slots;
pub mod tests;
pub mod trust;
pub mod why;
