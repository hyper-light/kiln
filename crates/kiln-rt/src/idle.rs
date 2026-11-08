//! What a pool's worker does as it starts, and once it has waited a while with nothing to do (kiln's goal 11, plan
//! 01a1a87b2e56 step 5): the process's allocator may keep memory for each thread, trimmed only as that thread
//! allocates (jemalloc's thread caches: their garbage collection is driven by the thread's own allocations), and a
//! thread's stack keeps every page it touched at its deepest, so a worker that ran a job and then waits keeps what
//! its job freed and the stack its job reached for as long as it waits. The consumer says once, as it starts, how
//! long a wait makes a worker cold and what a cold worker does (kiln: give its cache and its stack's unused pages
//! back); the blocking pool's and the compute pool's workers do it once a wait, before they wait on. It may say too
//! what a worker does as it starts, before its first job (kiln: its stack wants no huge pages; the blocking pool's
//! workers take an arena of their own where a run asks for one).

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

/// The pool a worker serves, as it's told to what it does as it starts ([`when_started`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pool {
    /// The blocking pool's ([`crate::blocking`]): calls that block a thread, most of its workers waiting.
    Blocking,
    /// The compute pool's ([`crate::compute`]): CPU work off the shards.
    Compute,
}

static STARTED: OnceLock<fn(Pool)> = OnceLock::new();

/// Sets, once for the process, what a pool's worker does as it starts, on its own thread, before its first job:
/// false when it was set already. A pool started before it's set has its workers start as before.
pub fn when_started(then: fn(Pool)) -> bool {
    STARTED.set(then).is_ok()
}

/// What a worker of `pool` does as it starts, if the process said: on the worker's own thread.
pub(crate) fn started(pool: Pool) {
    if let Some(then) = STARTED.get() {
        then(pool);
    }
}

static SPARE: OnceLock<fn() -> bool> = OnceLock::new();

/// Sets, once for the process, a piece of work the compute pool's workers do with spare capacity alone: a worker that finds
/// no job takes one piece (`then`, on its own thread) before it rests, and looks for jobs again after it, so no job waits
/// behind more than one piece and none waits for a wake of its (kiln's consts-audit R3: runners' payloads looked at for
/// fields this build doesn't know, which waited for a thread of their own that slept a tenth of a second at a time).
/// `then` says whether it found a piece to do: none, and the worker rests. False when it was set already. No lock: a
/// `OnceLock`, read as each worker finds nothing to do.
pub fn when_spare(then: fn() -> bool) -> bool {
    SPARE.set(then).is_ok()
}

/// A piece of the process's spare work, done on this worker's thread ([`when_spare`]): whether there was one.
pub(crate) fn spare() -> bool {
    SPARE.get().is_some_and(|then| then())
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;

    static BLOCKING: AtomicUsize = AtomicUsize::new(0);
    static COMPUTE: AtomicUsize = AtomicUsize::new(0);

    /// This test's own threads' starts alone: the pools other tests start in this process start their workers too.
    const NAME: &str = "g11-s5-started";

    fn counted(pool: Pool) {
        if std::thread::current().name() != Some(NAME) {
            return;
        }
        match pool {
            Pool::Blocking => BLOCKING.fetch_add(1, Ordering::SeqCst),
            Pool::Compute => COMPUTE.fetch_add(1, Ordering::SeqCst),
        };
    }

    fn other(_: Pool) {}

    /// Kiln's goal 11, plan 01a1a87b2e56 step 5: what a worker does as it starts is said once for the process, and
    /// each worker that starts does it on its own thread, told its pool.
    #[test]
    fn g11_s5_a_worker_does_as_the_process_said_as_it_starts() {
        assert!(when_started(counted));
        assert!(!when_started(other), "said once for the process");
        for pool in [Pool::Blocking, Pool::Compute, Pool::Blocking] {
            std::thread::Builder::new().name(NAME.into()).spawn(move || started(pool)).unwrap().join().unwrap();
        }
        started(Pool::Compute);
        assert_eq!(BLOCKING.load(Ordering::SeqCst), 2);
        assert_eq!(COMPUTE.load(Ordering::SeqCst), 1, "on the worker's own thread, never the caller's");
    }
}
