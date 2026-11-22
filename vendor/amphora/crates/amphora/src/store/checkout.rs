//! Amphora (G3 step 6, prototype F): a read takes its reader without a lock or a mutex every reader
//! shares (docs/design/store.md decision 43; docs/research/read-path.md, the 32-client profile).
//!
//! The pool (`Readers::with`'s other way) takes the pool's mutex to take a reader and again to give
//! it back, wakes the waiters, and holds the read side of one `RwLock` for the read: three writes
//! per read to lines every reader writes, which move between their cores at 32 clients (decision
//! 22's effect, LeanStore's rule: "programs that frequently write to memory locations accessed by
//! multiple threads do not scale"). Here each reader lives in a slot of its own, on a line of its
//! own; a thread takes the slot its number names, so at most as many threads as slots each write
//! only their own slot's lock. A thread whose slot is busy tries the others; when all are, it waits
//! on the pool's condition variable, and a read that ends wakes it only when someone waits (a read
//! of a counter only waiters write).
//!
//! Holding a slot is what holding the gate's read side was: the writer's restart of the log and
//! the readers' release take every slot, so no read is under way meanwhile.

use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering, fence};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::Instant;

/// How a read takes its reader (the prototype's switch; `AMPHORA_READ_CHECKOUT` sets it for a
/// process: `pool` or `slots`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum ReadCheckout {
    /// Today's: one pool behind a mutex, and a shared gate held through each read.
    Pool = 0,
    /// A slot a reader, on a line of its own, taken by the thread whose number names it.
    Slots = 1,
}

/// `set_process_default`'s choice; `UNSET` until it is called.
static PROCESS_DEFAULT: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(UNSET);
const UNSET: u8 = u8::MAX;

impl ReadCheckout {
    /// The way stores started from now on take: `set_process_default`'s if called, else
    /// `AMPHORA_READ_CHECKOUT` (read once), else `Pool`.
    pub fn process_default() -> Self {
        static FROM_ENV: std::sync::OnceLock<ReadCheckout> = std::sync::OnceLock::new();
        match PROCESS_DEFAULT.load(Ordering::Relaxed) {
            UNSET => {
                *FROM_ENV.get_or_init(|| match std::env::var("AMPHORA_READ_CHECKOUT").as_deref() {
                    Ok("slots") => Self::Slots,
                    _ => Self::Pool,
                })
            }
            1 => Self::Slots,
            _ => Self::Pool,
        }
    }

    /// Sets the way stores started from now on in this process take (a bench's or a test's switch).
    pub fn set_process_default(mode: Self) {
        PROCESS_DEFAULT.store(mode as u8, Ordering::Relaxed);
    }
}

/// A slot, alone on its cache line pair (the adjacent-line prefetcher pulls lines in pairs, so
/// 128 bytes, as crossbeam's `CachePadded` aligns on x86_64 and aarch64).
#[repr(align(128))]
struct Slot<R>(Mutex<Option<R>>);

/// The slots and what a reader waiting for one waits on.
pub(super) struct Slots<R> {
    slots: Box<[Slot<R>]>,
    /// Threads waiting for a slot: written by waiters alone, read by each read as it ends.
    waiting: AtomicUsize,
    wait: Mutex<()>,
    freed: Condvar,
}

/// A slot taken: its reader (None until one is opened in it) and the slots, to wake a waiter when
/// it is given back.
pub(super) struct Taken<'a, R> {
    slot: Option<MutexGuard<'a, Option<R>>>,
    slots: &'a Slots<R>,
}

impl<R> Taken<'_, R> {
    /// The slot's reader: None until one is opened in it.
    pub(super) fn reader(&mut self) -> Option<&mut Option<R>> {
        self.slot.as_deref_mut()
    }
}

impl<R> Drop for Taken<'_, R> {
    fn drop(&mut self) {
        // The slot unlocked first, then a waiter woken, so the waiter finds it free.
        drop(self.slot.take());
        self.slots.wake();
    }
}

/// Each thread's number, taken once from a counter (a write to a shared line once per thread,
/// never per read).
fn thread_number() -> usize {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    thread_local! {
        static NUMBER: Cell<Option<usize>> = const { Cell::new(None) };
    }
    NUMBER.with(|n| match n.get() {
        Some(v) => v,
        None => {
            let v = NEXT.fetch_add(1, Ordering::Relaxed);
            n.set(Some(v));
            v
        }
    })
}

impl<R> Slots<R> {
    /// `most` slots, at least one.
    pub(super) fn new(most: usize) -> Self {
        Self {
            slots: (0..most.max(1)).map(|_| Slot(Mutex::new(None))).collect(),
            waiting: AtomicUsize::new(0),
            wait: Mutex::new(()),
            freed: Condvar::new(),
        }
    }

    fn lock(slot: &Slot<R>) -> MutexGuard<'_, Option<R>> {
        slot.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// One free slot, this thread's first, without waiting.
    fn try_take(&self) -> Option<MutexGuard<'_, Option<R>>> {
        let len = self.slots.len();
        let first = thread_number().checked_rem(len).unwrap_or(0);
        let (earlier, from_first) = self.slots.split_at(first.min(len));
        from_first
            .iter()
            .chain(earlier)
            .find_map(|slot| match slot.0.try_lock() {
                Ok(guard) => Some(guard),
                Err(TryLockError::Poisoned(p)) => Some(p.into_inner()),
                Err(TryLockError::WouldBlock) => None,
            })
    }

    /// A slot: a free one at once, or the first given back before `until` (None past it).
    pub(super) fn take(&self, until: Instant) -> Option<Taken<'_, R>> {
        if let Some(reader) = self.try_take() {
            return Some(Taken {
                slot: Some(reader),
                slots: self,
            });
        }
        self.waiting.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        let mut wait = self.wait.lock().unwrap_or_else(PoisonError::into_inner);
        let taken = loop {
            if let Some(reader) = self.try_take() {
                break Some(reader);
            }
            let left = until.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break None;
            }
            wait = self
                .freed
                .wait_timeout(wait, left)
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        };
        drop(wait);
        self.waiting.fetch_sub(1, Ordering::SeqCst);
        taken.map(|reader| Taken {
            slot: Some(reader),
            slots: self,
        })
    }

    /// Wakes the waiters, if any. Called as a slot is given back: the waiter counts itself before
    /// it tries the slots under `wait`, and this reads the count after the slot's unlock (the
    /// fence), so a waiter whose try failed is either seen here or tries again after.
    fn wake(&self) {
        fence(Ordering::SeqCst);
        if self.waiting.load(Ordering::SeqCst) > 0 {
            drop(self.wait.lock().unwrap_or_else(PoisonError::into_inner));
            self.freed.notify_all();
        }
    }

    /// Every slot, each once no read holds it: no read is under way while they are held, and the
    /// reads that came meanwhile are woken when they are given back.
    pub(super) fn take_all(&self) -> All<'_, R> {
        All {
            held: self.slots.iter().map(Self::lock).collect(),
            slots: self,
        }
    }
}

/// Every slot held (`Slots::take_all`, by the log's restart and the readers' release). A read that
/// found them all held waits on the condition variable, and no read ends to wake it while they
/// are: giving them back wakes it.
pub(super) struct All<'a, R> {
    held: Vec<MutexGuard<'a, Option<R>>>,
    slots: &'a Slots<R>,
}

impl<R> All<'_, R> {
    /// Each slot's reader (None where none was opened).
    pub(super) fn readers(&mut self) -> impl Iterator<Item = &mut Option<R>> {
        self.held.iter_mut().map(|slot| &mut **slot)
    }
}

impl<R> Drop for All<'_, R> {
    fn drop(&mut self) {
        // Every slot unlocked first, then the waiters woken, so they find them free.
        self.held.clear();
        self.slots.wake();
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Readers, Refused, locked};
    use super::ReadCheckout;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, Instant};

    /// A reader: its number, and a flag set while a read uses it.
    struct Reader {
        busy: Arc<AtomicBool>,
        dropped: Arc<AtomicUsize>,
    }

    impl Drop for Reader {
        fn drop(&mut self) {
            self.dropped.fetch_add(1, Ordering::SeqCst);
        }
    }

    fn readers(most: usize) -> (Arc<Readers<Reader>>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let readers = Arc::new(Readers::with_checkout(
            most,
            Arc::default(),
            ReadCheckout::Slots,
        ));
        let (opened, dropped) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
        let (o, d) = (opened.clone(), dropped.clone());
        *locked(&readers.open) = Some(Arc::new(move || {
            o.fetch_add(1, Ordering::SeqCst);
            Ok(Reader {
                busy: Arc::new(AtomicBool::new(false)),
                dropped: d.clone(),
            })
        }));
        (readers, opened, dropped)
    }

    fn far() -> Instant {
        Instant::now() + Duration::from_secs(600)
    }

    /// G3 step 6 (prototype F, T-G3.6.3). Do: 16 threads read 500 times each through 4 slots. Expect: every
    /// read answered, at most 4 readers opened, and no reader used by two reads at once.
    #[test]
    fn g3_s6_slot_readers_serve_many_threads_one_read_a_reader_at_a_time() {
        let (readers, opened, _) = readers(4);
        let threads: Vec<_> = (0..16)
            .map(|_| {
                let readers = readers.clone();
                std::thread::spawn(move || {
                    for _ in 0..500 {
                        let answer = readers.with(far(), |r| {
                            let was = r.busy.swap(true, Ordering::SeqCst);
                            std::hint::spin_loop();
                            r.busy.store(false, Ordering::SeqCst);
                            Ok(was)
                        });
                        assert!(
                            !answer.unwrap().unwrap(),
                            "a reader used by two reads at once"
                        );
                    }
                })
            })
            .collect();
        for t in threads {
            t.join().unwrap();
        }
        assert!(opened.load(Ordering::SeqCst) <= 4);
    }

    /// T-G3.6.3. Do: hold the one slot in a read; read from another thread with no time left, then with
    /// time; end the first read. Expect: the first try refused as a timeout; the second answered
    /// once the slot is given back (a waiter is woken, never left to its deadline).
    #[test]
    fn g3_s6_a_read_waits_for_a_slot_and_is_woken_when_one_is_given_back() {
        let (readers, _, _) = readers(1);
        let (hold_tx, hold_rx) = mpsc::channel::<()>();
        let (held_tx, held_rx) = mpsc::channel::<()>();
        let holder = {
            let readers = readers.clone();
            std::thread::spawn(move || {
                readers.with(far(), |_| {
                    held_tx.send(()).unwrap();
                    hold_rx.recv().unwrap();
                    Ok(())
                })
            })
        };
        held_rx.recv().unwrap();
        let refused = readers.with(Instant::now(), |_| Ok(()));
        assert!(matches!(
            refused.unwrap().unwrap_err().downcast_ref::<Refused>(),
            Some(Refused::Timeout)
        ));
        let waiter = {
            let readers = readers.clone();
            std::thread::spawn(move || readers.with(far(), |_| Ok(7)))
        };
        // The waiter counts itself before it waits; let the holder go once it has.
        while readers
            .slots
            .as_ref()
            .unwrap()
            .waiting
            .load(Ordering::SeqCst)
            == 0
        {
            std::thread::yield_now();
        }
        hold_tx.send(()).unwrap();
        holder.join().unwrap().unwrap().unwrap();
        assert_eq!(waiter.join().unwrap().unwrap().unwrap(), 7);
    }

    /// T-G3.6.3. Do: hold every slot (as the log's restart does), read from another thread, then
    /// give the slots back. Expect: the read answered once they are given back (the restart's end
    /// wakes a waiting read; none is left to its deadline).
    #[test]
    fn g3_s6_a_read_held_back_by_a_restart_is_woken_when_it_ends() {
        let (readers, _, _) = readers(2);
        let slots = readers.slots.as_ref().unwrap();
        std::thread::scope(|s| {
            let all = slots.take_all();
            let waiter =
                s.spawn(|| readers.with(Instant::now() + Duration::from_secs(60), |_| Ok(7)));
            while slots.waiting.load(Ordering::SeqCst) == 0 {
                std::thread::yield_now();
            }
            drop(all);
            assert_eq!(waiter.join().unwrap().unwrap().unwrap(), 7);
        });
    }

    /// T-G3.6.3. Do: read on 3 slots, then release the readers, then read again. Expect: every reader
    /// opened is let go by the release, and a read after it finds no reader (the writer reads).
    #[test]
    fn g3_s6_release_lets_every_slot_reader_go() {
        let (readers, opened, dropped) = readers(3);
        std::thread::scope(|s| {
            for _ in 0..3 {
                s.spawn(|| {
                    for _ in 0..50 {
                        readers.with(far(), |_| Ok(())).unwrap().unwrap();
                    }
                });
            }
        });
        readers.release();
        assert_eq!(
            dropped.load(Ordering::SeqCst),
            opened.load(Ordering::SeqCst)
        );
        assert!(readers.with(far(), |_| Ok(())).is_none());
    }
}
