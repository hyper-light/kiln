//! The bytes the QUIC transport may hold, on the server and on a runner (`crate::net`), and the buffers it
//! keeps for reuse (goal 11 of the API runtime program, plan 01a1a87b2e56 step 5; goal 11's audit, row B22).
//!
//! hyper-transport's `Fixed` recycles every buffer its reservations are given back with, up to a count, each
//! as large as the largest frame it ever carried: in the lab's realistic profile its spares held 8.3 to 8.8 MB
//! of the server's live memory (docs/runtime-lab.md, "Where the memory goes"), Bulk frames' 4 MiB buffers among
//! them, kept for as long as the server ran. Bounded instead at the Interactive class's largest frame (1 MiB),
//! a Bulk frame past every spare was allocated fresh on the net thread, and its pages faulted in there once the
//! allocator had given them back, while every runner's frames waited behind it. [`Spares`] keeps what the frames
//! this process sees have needed at once: as many buffers, and as many bytes, as its frames have had out at once at
//! the most, learned as they come. A pattern of frames seen before finds its buffers kept, whatever their class;
//! the spares hold no more than the busiest moment's frames did.

use hyper_transport::{Budget, Lane, Refusal, Reservation};

use crate::net::Class;

/// A budget of `capacity` bytes, refusing a reservation past it, that keeps given-back buffers for reuse, as many
/// and as large in all as the frames have had out at once.
#[derive(Debug)]
pub struct Spares {
    capacity: u64,
    used: u64,
    /// Buffers kept for reuse.
    spare: Vec<Vec<u8>>,
    /// Their capacities added up: never past `peak_bytes`.
    held: usize,
    /// The buffers frames hold now: how many, and their capacities added up.
    out: usize,
    out_bytes: usize,
    /// The most buffers frames have held at once, and the most bytes: what the spares keep at most.
    peak: usize,
    peak_bytes: usize,
    refused: u64,
    /// Frames whose buffer was allocated or grown, no spare holding them.
    fresh: u64,
}

impl Spares {
    /// A budget of `capacity` bytes, its spares learned from the frames it serves.
    pub fn new(capacity: u64) -> Self {
        Self { capacity, used: 0, spare: Vec::new(), held: 0, out: 0, out_bytes: 0, peak: 0, peak_bytes: 0, refused: 0, fresh: 0 }
    }

    /// The bytes reserved and not yet given back.
    pub fn used(&self) -> u64 {
        self.used
    }

    /// The reservations refused so far.
    pub fn refused(&self) -> u64 {
        self.refused
    }

    /// What the spares hold now, in bytes.
    pub fn held(&self) -> usize {
        self.held
    }

    /// The frames whose buffer was allocated or grown so far: none for a frame a spare held.
    pub fn fresh(&self) -> u64 {
        self.fresh
    }

    /// The spare at `at`, taken out of the pool.
    fn take(&mut self, at: usize) -> Option<Vec<u8>> {
        (at < self.spare.len()).then(|| {
            let buffer = self.spare.swap_remove(at);
            self.held = self.held.saturating_sub(buffer.capacity());
            buffer
        })
    }
}

impl Budget<Class> for Spares {
    fn reserve(&mut self, bytes: u64, lane: Lane<Class>) -> Result<Reservation, Refusal> {
        let used = self.used.checked_add(bytes).filter(|used| *used <= self.capacity);
        let (Some(used), Ok(wanted)) = (used, usize::try_from(bytes)) else {
            self.refused = self.refused.saturating_add(1);
            return Err(Refusal::Budget);
        };
        self.used = used;
        let buffer = match lane {
            // Accounting only: no buffer.
            Lane::Window => Vec::new(),
            Lane::Class(_) => {
                // The smallest spare that holds the grant, or else the largest, as `Fixed` takes them: a buffer that
                // already holds the grant is never grown.
                let fits = self.spare.iter().enumerate().filter(|(_, s)| s.capacity() >= wanted).min_by_key(|(_, s)| s.capacity()).map(|(at, _)| at);
                let largest = || self.spare.iter().enumerate().max_by_key(|(_, s)| s.capacity()).map(|(at, _)| at);
                let mut buffer = fits.or_else(largest).and_then(|at| self.take(at)).unwrap_or_default();
                if buffer.capacity() < wanted {
                    self.fresh = self.fresh.saturating_add(1);
                }
                buffer.clear();
                buffer.reserve_exact(wanted);
                if buffer.capacity() > 0 {
                    self.out = self.out.saturating_add(1);
                    self.out_bytes = self.out_bytes.saturating_add(buffer.capacity());
                    self.peak = self.peak.max(self.out);
                    self.peak_bytes = self.peak_bytes.max(self.out_bytes);
                }
                buffer
            }
        };
        Ok(Reservation::new(bytes, buffer))
    }

    fn release(&mut self, reservation: Reservation) {
        self.used = self.used.saturating_sub(reservation.granted());
        let buffer = reservation.into_buffer();
        let capacity = buffer.capacity();
        if capacity == 0 {
            return;
        }
        self.out = self.out.saturating_sub(1);
        self.out_bytes = self.out_bytes.saturating_sub(capacity);
        // Kept while the spares hold no more than the frames have had out at once.
        if self.held.saturating_add(capacity) <= self.peak_bytes && self.spare.len() < self.peak {
            self.held = self.held.saturating_add(capacity);
            self.spare.push(buffer);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn refused(e: Refusal) -> anyhow::Error {
        anyhow::anyhow!("refused: {e:?}")
    }

    /// A Bulk frame as a feed's part is (3 MiB, `crate::harness::live`'s parts up to 3 MiB), and an Interactive
    /// one.
    const BULK: u64 = 3 << 20;
    const SMALL: u64 = 4096;

    /// Goal 11's audit, row B22: a Bulk frame finds its buffer kept once a frame as large has been seen, where a
    /// spare bound at the Interactive class's largest frame (1 MiB, 1ff58bc6631) had every one allocated afresh on
    /// the net thread. One at a time among small frames: the first allocates, none after. Two at once: the first
    /// pair allocates one more, none after. The allocations a Bulk frame makes are counted as no spare holds it.
    #[test]
    fn g11_b22_a_bulk_frame_finds_its_buffer_once_one_as_large_was_seen() -> anyhow::Result<()> {
        let mut budget = Spares::new(1 << 30);
        let mut each = Vec::new();
        for _ in 0..5 {
            let before = budget.fresh();
            let small = budget.reserve(SMALL, Lane::Class(Class::Interactive)).map_err(refused)?;
            let bulk = budget.reserve(BULK, Lane::Class(Class::Bulk)).map_err(refused)?;
            budget.release(small);
            budget.release(bulk);
            each.push(budget.fresh() - before);
        }
        anyhow::ensure!(each == [2, 0, 0, 0, 0], "frames allocated afresh each round: {each:?}");
        let mut pairs = Vec::new();
        for _ in 0..4 {
            let before = budget.fresh();
            let (a, b) = (budget.reserve(BULK, Lane::Class(Class::Bulk)).map_err(refused)?, budget.reserve(BULK, Lane::Class(Class::Bulk)).map_err(refused)?);
            budget.release(a);
            budget.release(b);
            pairs.push(budget.fresh() - before);
        }
        anyhow::ensure!(pairs == [1, 0, 0, 0], "two Bulk frames at once, allocated afresh each round: {pairs:?}");
        anyhow::ensure!(budget.used() == 0);
        Ok(())
    }

    /// Goal 11's audit, row B22: the spares hold no more buffers, and no more bytes, than the frames have had out at
    /// once: a burst of a hundred small frames is kept whole for the next, and what's past the busiest moment goes
    /// back to the allocator.
    #[test]
    fn g11_b22_the_spares_hold_what_the_frames_had_out_at_once_and_no_more() -> anyhow::Result<()> {
        let mut budget = Spares::new(1 << 30);
        let small = budget.reserve(1000, Lane::Class(Class::Interactive)).map_err(refused)?;
        let pointer = small.bytes().as_ptr();
        budget.release(small);
        anyhow::ensure!(budget.held() == 1000 && budget.used() == 0, "{}", budget.held());
        let again = budget.reserve(900, Lane::Class(Class::Interactive)).map_err(refused)?;
        anyhow::ensure!(again.bytes().as_ptr() == pointer && budget.held() == 0, "the spare was reused, taken out of the pool");
        budget.release(again);
        // A hundred at once: kept whole for the next hundred, which allocate nothing.
        let burst = |budget: &mut Spares| -> anyhow::Result<u64> {
            let before = budget.fresh();
            let held = (0..100).map(|_| budget.reserve(4096, Lane::Class(Class::Interactive)).map_err(refused)).collect::<anyhow::Result<Vec<_>>>()?;
            for r in held {
                budget.release(r);
            }
            Ok(budget.fresh() - before)
        };
        anyhow::ensure!(burst(&mut budget)? == 100, "the first burst allocates each, the one spare grown");
        anyhow::ensure!(budget.spare.len() == 100 && budget.held() <= 100 * 4096 + 1000, "{} spares, {} bytes", budget.spare.len(), budget.held());
        anyhow::ensure!(burst(&mut budget)? == 0, "the next finds them kept");
        // Never past the busiest moment: a buffer given back over it goes.
        let (peak, peak_bytes) = (budget.peak, budget.peak_bytes);
        let extra = Reservation::new(0, Vec::with_capacity(4096));
        budget.out = budget.out.saturating_add(1);
        budget.out_bytes = budget.out_bytes.saturating_add(4096);
        budget.release(extra);
        anyhow::ensure!(budget.spare.len() <= peak && budget.held() <= peak_bytes, "{} of {peak}, {} of {peak_bytes}", budget.spare.len(), budget.held());
        anyhow::ensure!(budget.used() == 0 && budget.out == 0 && budget.out_bytes == 0);
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: the budget refuses past its capacity as `Fixed` does, and a window's
    /// reservation carries no buffer and counts as no frame's.
    #[test]
    fn g11_s5_spares_refuse_past_their_capacity() -> anyhow::Result<()> {
        let mut budget = Spares::new(100);
        let first = budget.reserve(60, Lane::Class(Class::Interactive)).map_err(refused)?;
        anyhow::ensure!(budget.reserve(41, Lane::Class(Class::Interactive)).err() == Some(Refusal::Budget));
        anyhow::ensure!((budget.used(), budget.refused()) == (60, 1));
        budget.release(first);
        anyhow::ensure!(budget.used() == 0);
        let window = budget.reserve(60, Lane::Window).map_err(refused)?;
        anyhow::ensure!(window.granted() == 60 && window.room() == 60 && window.bytes().is_empty());
        budget.release(window);
        anyhow::ensure!((budget.peak, budget.out) == (1, 0), "a window's reservation is no frame's");
        Ok(())
    }
}
