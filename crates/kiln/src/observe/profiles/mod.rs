//! Continuous profiles (`docs/observe.md` §8.11), Datadog's Continuous Profiler: what tracers'
//! profilers upload (`intake`: `multipart/form-data`, read by `multipart`), its pprof read and
//! summed by `kiln_obs::profile`.

pub mod intake;
pub mod multipart;
