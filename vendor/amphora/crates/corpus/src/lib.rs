//! The competition corpus's runner (G2 steps 4 and 7; bench/corpus): sqllogictest records read,
//! run, rendered and compared as SQLite's own runner does, one code path for the gate's subset on
//! Amphora and the bench's differential run over every engine. Disagreements are classified as
//! the step asks: an Amphora bug, a competitor's bug, a dialect difference or a translation gap.
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

pub mod amphora;
pub mod answer;
pub mod classify;
pub mod complete;
pub mod dialect;
pub mod engines;
pub mod md5;
pub mod run;
pub mod slt;
