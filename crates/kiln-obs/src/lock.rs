//! A read-write lock that prefers a waiting writer on every platform (`docs/observe.md` §4): a store
//! is written by its machine an entry at a time and read by queries, and a writer must never wait on
//! readers that keep coming. std's lock prefers a waiting writer on Linux (a futex) but not on macOS
//! (its queue) nor Windows (an SRW lock, neither fair nor in order): there a query read over and over
//! held a write off for good. This is `parking_lot`'s, task-fair: a reader waits while a writer does,
//! even with the lock free. `read` and `write` answer as std's do (never poisoned: a holder that
//! panicked leaves the lock as it was), so callers read them alike.

use std::sync::LockResult;

/// What [`RwLock::read`] holds.
pub type ReadGuard<'a, T> = parking_lot::RwLockReadGuard<'a, T>;
/// What [`RwLock::write`] holds.
pub type WriteGuard<'a, T> = parking_lot::RwLockWriteGuard<'a, T>;

#[derive(Default)]
pub struct RwLock<T>(parking_lot::RwLock<T>);

impl<T> RwLock<T> {
    pub const fn new(value: T) -> Self {
        Self(parking_lot::RwLock::new(value))
    }

    /// Held for reading once no writer holds it or waits for it.
    pub fn read(&self) -> LockResult<ReadGuard<'_, T>> {
        Ok(self.0.read())
    }

    /// Held for writing once no one holds it; readers that come meanwhile wait behind.
    pub fn write(&self) -> LockResult<WriteGuard<'_, T>> {
        Ok(self.0.write())
    }

    pub fn into_inner(self) -> LockResult<T> {
        Ok(self.0.into_inner())
    }

    pub fn get_mut(&mut self) -> LockResult<&mut T> {
        Ok(self.0.get_mut())
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for RwLock<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::time::{Duration, Instant};

    /// A writer is let in beside a reader that takes the lock again the moment it lets it go (and
    /// readers beside it): each of its writes waits for one read at the most, never for good.
    #[test]
    fn a_writer_is_let_in_between_readers_that_keep_coming() {
        let lock = RwLock::new(0u64);
        let stop = AtomicBool::new(false);
        let reads = AtomicU64::new(0);
        std::thread::scope(|s| {
            for _ in 0..3 {
                s.spawn(|| {
                    while !stop.load(Ordering::Relaxed) {
                        let g = lock.read().unwrap();
                        let t = Instant::now();
                        while t.elapsed() < Duration::from_micros(200) {
                            std::hint::black_box(*g);
                        }
                        drop(g);
                        reads.fetch_add(1, Ordering::Relaxed);
                    }
                });
            }
            let t = Instant::now();
            for i in 1..=200 {
                *lock.write().unwrap() = i;
                std::thread::sleep(Duration::from_micros(100));
            }
            stop.store(true, Ordering::Relaxed);
            assert!(t.elapsed() < Duration::from_secs(20), "200 writes took {:?} beside readers ({} reads)", t.elapsed(), reads.load(Ordering::Relaxed));
        });
        assert_eq!(*lock.read().unwrap(), 200);
    }
}
