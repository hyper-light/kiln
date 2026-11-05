//! What a pool's worker does once it has waited a while with nothing to do (kiln's goal 11, plan 01a1a87b2e56
//! step 5): the process's allocator may keep memory for each thread, trimmed only as that thread allocates
//! (jemalloc's thread caches: their garbage collection is driven by the thread's own allocations), so a worker
//! that ran a job and then waits keeps what its job freed for as long as it waits. The consumer says once, as
//! it starts, how long a wait makes a worker cold and what a cold worker does (kiln: give its cache back);
//! the blocking pool's and the compute pool's workers do it once a wait, before they wait on.

use std::sync::OnceLock;
use std::time::Duration;

/// How long a worker waits with nothing to do before it's cold, and what it does then, on its own thread.
#[derive(Clone, Copy, Debug)]
pub struct Cold {
    /// The wait that makes a worker cold.
    pub after: Duration,
    /// What a cold worker does, once a wait, on its own thread.
    pub then: fn(),
}

static COLD: OnceLock<Cold> = OnceLock::new();

/// Sets, once for the process, what a pool's worker does after waiting `after` with nothing to do: false when
/// it was set already. Without it a worker waits as before, with nothing to do when it's cold.
pub fn when_cold(after: Duration, then: fn()) -> bool {
    COLD.set(Cold { after, then }).is_ok()
}

/// What a worker does once it's cold, if the process said.
pub(crate) fn cold() -> Option<Cold> {
    COLD.get().copied()
}
