//! Continuous profiles (`docs/observe.md` §8.11): what Datadog's tracers' profilers upload (pprof),
//! read in place (`pprof`) and summed into flame graphs (`flame`).

pub mod flame;
pub mod pprof;
