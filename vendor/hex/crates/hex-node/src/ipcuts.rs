//! The node agent's ipc and uts namespaces made ahead (goal 5 extended, step 4): the shell around
//! [`hex_core::netns::Pool`], as [`crate::netns`] is for network namespaces. One thread of the agent makes each
//! pair off the create path once the pool is down to its mark ([`low`]), up to its size ([`ipcuts::make_ahead`]),
//! which it learns from its draws (Ada's rule, 2026-11-06). A create routed to a pruned
//! seed takes one ([`IpcUtsAhead`]), which its init joins in place of its clone making them, and the pool counts
//! every other container that makes either namespace as made on the path. A pair taken is the container's alone,
//! so it ends with the container.

use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::JoinHandle;

use hex_api::Refusal;
use hex_core::netns::{Counts, Pool, Reason, Step, Take};
use hex_runtime::linux::oci::OciError;
use hex_runtime::linux::oci::ipcuts::{self, Pair};
use hex_runtime::linux::oci::run::IpcUtsAhead;

/// The pool and the thread that fills it. Dropped, it shuts the pool down, which closes what it held, and waits
/// for the thread, which ends once the make it has under way is done.
pub struct IpcUtsPool {
    shared: Arc<Shared>,
    refill: Option<JoinHandle<()>>,
}

struct Shared {
    pool: Mutex<Pool<Pair>>,
    /// The refill thread waits on it while the pool wants nothing. A take that leaves the pool at its mark ([`low`])
    /// or under, and shutdown, wake it.
    wake: Condvar,
}

/// The fastest the creates have taken pairs on record: one a create, at the sustained phase's 10,383 a second
/// without a network namespace, the fastest Hex has run (`bench/competitive/probes/create_serial.md`, run 20).
const TAKES_PER_SECOND: u64 = 10_383;
/// The slowest the refill has made pairs on record: one a 0.035 ms, the p99 make on fedora
/// (`bench/competitive/probes/netns_pool.md`, goal 5 extended step 4), 28,571 a second.
const MAKES_PER_SECOND: u64 = 28_571;

/// The mark a take wakes the refill at, for a pool of `size`: the fewest pairs that, taken at the fastest rate on
/// record, outlast the refill's making of the rest at its slowest, `size x T / (T + M)` rounded up, and one at
/// least in a pool of any. A wake is a system call on create's path (Rust's futex `Condvar` makes one with or
/// without a waiter), so takes above the mark make none; a draw that still finds the pool short raises its size
/// ([`hex_core::netns::Pool::learning`]), and the mark with it.
fn low(size: usize) -> usize {
    let size64 = u64::try_from(size).unwrap_or(u64::MAX);
    let whole = TAKES_PER_SECOND.saturating_add(MAKES_PER_SECOND);
    let mark = size64
        .saturating_mul(TAKES_PER_SECOND)
        .saturating_add(whole.saturating_sub(1))
        .checked_div(whole)
        .unwrap_or(size64);
    usize::try_from(mark).unwrap_or(size).min(size)
}

impl Shared {
    fn pool(&self) -> Result<MutexGuard<'_, Pool<Pair>>, OciError> {
        self.pool.lock().map_err(|_| {
            OciError::Broken("the ipc and uts namespace pool's lock was poisoned".to_owned())
        })
    }
}

fn shut_down() -> OciError {
    OciError::Broken("the ipc and uts namespace pool is shut down".to_owned())
}

impl IpcUtsPool {
    /// A pool that holds `bound` pairs, and the thread that starts filling it at once. With a bound of 0 there is
    /// no thread, and every container's clone makes its own, counted, as a rootless node's do.
    #[cfg(any(test, feature = "test-controls"))]
    pub fn start(bound: usize) -> Result<Self, Refusal> {
        Self::with_pool(Pool::new(bound), bound)
    }

    /// A pool that starts at `start` pairs and learns its size from its draws, up to `cap`
    /// ([`hex_core::netns::Pool::learning`]): a node's default, its start the host's parallelism and its cap the
    /// node's `ipc_uts` ([`crate::sizes::Sizes`]).
    pub fn learning(start: usize, cap: usize) -> Result<Self, Refusal> {
        Self::with_pool(Pool::learning(start, cap), cap)
    }

    fn with_pool(pool: Pool<Pair>, cap: usize) -> Result<Self, Refusal> {
        let shared = Arc::new(Shared {
            pool: Mutex::new(pool),
            wake: Condvar::new(),
        });
        if cap == 0 {
            return Ok(Self {
                shared,
                refill: None,
            });
        }
        let filling = Arc::clone(&shared);
        let refill = std::thread::Builder::new()
            .name("hex-ipcuts".to_owned())
            .spawn(move || refill(&filling))
            .map_err(|e| Refusal::os("starting the ipc and uts namespace thread", e))?;
        Ok(Self {
            shared,
            refill: Some(refill),
        })
    }

    /// Its size now, learned from its draws.
    pub fn bound(&self) -> Result<usize, Refusal> {
        Ok(self.shared.pool().map_err(Refusal::from)?.bound())
    }

    /// What the pool has done. Counted at the take, so a create that fails after it took a pair, or found the
    /// pool dry, is counted all the same.
    pub fn counts(&self) -> Result<Counts, Refusal> {
        Ok(self.shared.pool().map_err(Refusal::from)?.counts())
    }
}

impl IpcUtsAhead for IpcUtsPool {
    fn take(&self) -> Result<Option<Pair>, OciError> {
        let mut pool = self.shared.pool()?;
        let before = pool.bound();
        let taken = pool.take().map_err(|_| shut_down())?;
        if pool.bound() > before {
            crate::sizes::rose();
        }
        let wake = self.refill.is_some() && pool.held() <= low(pool.bound());
        drop(pool);
        if wake {
            self.shared.wake.notify_one();
        }
        Ok(match taken {
            Take::Pooled(pair) => Some(pair),
            Take::MakeOnPath => None,
        })
    }

    fn on_path(&self) -> Result<(), OciError> {
        self.shared.pool()?.on_path().map_err(|_| shut_down())
    }
}

impl Drop for IpcUtsPool {
    fn drop(&mut self) {
        if let Ok(mut pool) = self.shared.pool.lock() {
            // Closed as they go: namespaces never used.
            drop(pool.shut_down());
        }
        self.shared.wake.notify_all();
        if let Some(refill) = self.refill.take() {
            let _ = refill.join();
        }
    }
}

/// The refill thread: makes a pair whenever the pool asks for one and hands it in, one at a time, as the pool's
/// count of what it asks for needs, and ends once the pool is shut down. It opens its home pair first; a thread
/// that can't makes none, each make it is asked for failing, counted. A pair the pool refuses (past its bound,
/// which one maker never reaches, or after shutdown) is closed here. A poisoned lock ends it, and every create
/// then refuses on the same lock.
fn refill(shared: &Shared) {
    let home = Pair::of_this_thread();
    loop {
        let Ok(mut pool) = shared.pool.lock() else {
            return;
        };
        while !pool.is_shut_down() && pool.wanted() == 0 {
            pool = match shared.wake.wait(pool) {
                Ok(pool) => pool,
                Err(_) => return,
            };
        }
        if pool.is_shut_down() {
            return;
        }
        // Made with the lock let go, so a take never waits on a make.
        drop(pool);
        let made = match &home {
            Ok(home) => ipcuts::make_ahead(home),
            Err(e) => Err(e.clone()),
        };
        let Ok(mut pool) = shared.pool.lock() else {
            return;
        };
        match made {
            Ok(pair) => {
                let _ = pool.made(pair);
            }
            // The pair's make is one unshare and two opens, counted as the make's step (hex_core::netns::Step), its
            // reason the error's own text, so what failed isn't dropped (CLAUDE.md §3).
            Err(e) => pool.failed(Step::Unshare, Reason::new(None, &e.to_string())),
        }
    }
}

#[cfg(test)]
mod tests;
