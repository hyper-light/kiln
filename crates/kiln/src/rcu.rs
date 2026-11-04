//! Read-mostly state without a lock (goal 5 of the API runtime program, plan 01a1a59c2aae; research
//! 01a1a59c2aaf, findings f7 and f9): readers take the current copy, an `Arc` of an immutable value,
//! without waiting on anyone; a writer copies it, changes the copy and swaps it in whole (read, copy,
//! update). A reader holding a copy keeps it as it was while writers swap new ones in, and no reader
//! holds a writer up, nor a writer a reader: a thread the OS preempts while it holds a copy stalls no
//! one, where one preempted holding a lock stalls every shard that needs it (f2).
//!
//! For state that's read far more than it's written (the runners connected, their processes, the
//! series a metric has). What's written often stays with one owner (a task, a thread) and goes to it
//! as messages.

use std::sync::Arc;

use arc_swap::ArcSwap;

/// A value read without a lock and written by swapping a new copy in whole.
pub struct Rcu<T>(ArcSwap<T>);

impl<T> Rcu<T> {
    pub fn new(value: T) -> Self {
        Self(ArcSwap::from_pointee(value))
    }

    /// The current copy, for a short read (no reference count is taken). Not held across an
    /// `.await`: [`Rcu::get`] is for that.
    pub fn load(&self) -> arc_swap::Guard<Arc<T>> {
        self.0.load()
    }

    /// The current copy, to keep.
    pub fn get(&self) -> Arc<T> {
        self.0.load_full()
    }

    /// `value` from now on.
    pub fn set(&self, value: T) {
        self.0.store(Arc::new(value));
    }
}

impl<T: Clone> Rcu<T> {
    /// Changes it: `f` runs on a copy of the current value, which is swapped in whole unless another
    /// writer swapped theirs in first; then `f` runs again, on a copy of theirs. So `f` only changes
    /// the copy it's given, and says what it did by what it returns: the run that's swapped in is
    /// the one whose answer comes back. Lock-free: some writer always gets through.
    pub fn update<R>(&self, mut f: impl FnMut(&mut T) -> R) -> R {
        loop {
            let current = self.0.load_full();
            let mut next = T::clone(&current);
            let out = f(&mut next);
            let before = self.0.compare_and_swap(&current, Arc::new(next));
            if Arc::ptr_eq(&*before, &current) {
                return out;
            }
        }
    }
}

impl<T: Default> Default for Rcu<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Rcu<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Rcu").field(&*self.0.load()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Goal 5, plan 01a1a59c2aae step 2: a reader keeps the copy it took while a writer swaps a new
    /// one in, and the next read sees the new one.
    #[test]
    fn g5_a_reader_keeps_its_copy_while_a_writer_swaps() {
        let r = Rcu::new(vec![1, 2]);
        let held = r.get();
        let len = r.update(|v| {
            v.push(3);
            v.len()
        });
        assert_eq!(len, 3);
        assert_eq!(*held, vec![1, 2]);
        assert_eq!(*r.get(), vec![1, 2, 3]);
    }

    /// Writers on many threads at once: none is lost, as a lock would have kept them.
    #[test]
    fn g5_writers_at_once_lose_nothing() -> anyhow::Result<()> {
        let r = Arc::new(Rcu::new(0u64));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let r = Arc::clone(&r);
                std::thread::spawn(move || {
                    for _ in 0..1000 {
                        r.update(|n| *n += 1);
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().map_err(|_| anyhow::anyhow!("a writer panicked"))?;
        }
        assert_eq!(*r.get(), 8000);
        Ok(())
    }
}
