//! Doing the least work a change needs (docs/ci.md W24): every unit of work keyed by what it
//! depends on, so a recorded success can be reused and a rebuild can say why.
//!
//! - [`key`]: weak and strong fingerprints (W24-R01, R03 to R05).

pub mod key;
