//! The bytes the QUIC transport may hold, on the server and on a runner (`crate::net`), and the buffers it
//! keeps for reuse (goal 11 of the API runtime program, plan 01a1a87b2e56 step 5).
//!
//! hyper-transport's `Fixed` recycles every buffer its reservations are given back with, up to a count, each
//! as large as the largest frame it ever carried: in the lab's realistic profile its spares held 8.3 to 8.8 MB
//! of the server's live memory (docs/runtime-lab.md, "Where the memory goes"), Bulk frames' 4 MiB buffers among
//! them, kept for as long as the server ran. [`Spares`] is `Fixed` with the spares bounded in bytes:
//! [`SPARE_BYTES`], the most urgent class's largest frame, so a frame of any class but the largest Bulk ones
//! still reads into a buffer it finds, and what's past the bound goes back to the allocator, whose own caches
//! serve the next frames of a burst (a thread's cache for the small, freed pages kept for their decay for the
//! large: `crate::os::alloc`).

use hyper_transport::{Budget, Lane, Refusal, Reservation};

use crate::net::Class;

/// What the spares may hold in all, in bytes: the Interactive class's largest frame (`crate::net::Kiln`'s
/// `frame_bound`, 1 MiB; a unit test holds them equal), which one spare can take whole.
pub const SPARE_BYTES: u64 = 1 << 20;

/// A budget of `capacity` bytes, refusing a reservation past it, that keeps given-back buffers for reuse: at
/// most `max_spare` of them, [`SPARE_BYTES`] in all.
#[derive(Debug)]
pub struct Spares {
    capacity: u64,
    used: u64,
    /// Buffers kept for reuse.
    spare: Vec<Vec<u8>>,
    /// Their capacities added up: never past `spare_bytes`.
    held: usize,
    spare_bytes: usize,
    max_spare: usize,
    refused: u64,
}

impl Spares {
    /// A budget of `capacity` bytes that keeps at most `max_spare` buffers, [`SPARE_BYTES`] of them in all.
    pub fn new(capacity: u64, max_spare: usize) -> Self {
        Self::bounded(capacity, max_spare, usize::try_from(SPARE_BYTES).unwrap_or(usize::MAX))
    }

    /// [`Spares::new`] with the spares bounded at `spare_bytes`.
    fn bounded(capacity: u64, max_spare: usize, spare_bytes: usize) -> Self {
        Self { capacity, used: 0, spare: Vec::new(), held: 0, spare_bytes, max_spare, refused: 0 }
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
                buffer.clear();
                buffer.reserve_exact(wanted);
                buffer
            }
        };
        Ok(Reservation::new(bytes, buffer))
    }

    fn release(&mut self, reservation: Reservation) {
        self.used = self.used.saturating_sub(reservation.granted());
        let buffer = reservation.into_buffer();
        let capacity = buffer.capacity();
        let room = self.spare_bytes.saturating_sub(self.held);
        if capacity > 0 && capacity <= room && self.spare.len() < self.max_spare {
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

    /// Goal 11, plan 01a1a87b2e56 step 5: the spares hold at most the Interactive class's largest frame, 1 MiB,
    /// less than a Bulk frame's largest.
    #[test]
    fn g11_s5_the_spares_hold_the_most_urgent_classs_largest_frame() {
        use hyper_transport::Classes as _;
        assert_eq!(SPARE_BYTES, crate::net::Kiln::frame_bound(Class::Interactive));
        assert!(SPARE_BYTES < crate::net::Kiln::frame_bound(Class::Bulk), "a Bulk frame's largest buffer isn't kept");
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: a buffer given back is reused while the spares' bytes hold it; one past
    /// them goes back to the allocator, and the spares never hold more than their bound.
    #[test]
    fn g11_s5_spares_are_reused_within_their_bytes_and_let_go_past_them() -> anyhow::Result<()> {
        let mut budget = Spares::bounded(1 << 30, 256, 64 << 10);
        let small = budget.reserve(1000, Lane::Class(Class::Interactive)).map_err(refused)?;
        let pointer = small.bytes().as_ptr();
        budget.release(small);
        anyhow::ensure!(budget.used() == 0);
        anyhow::ensure!((1000..=64 << 10).contains(&budget.held()), "{}", budget.held());
        let again = budget.reserve(900, Lane::Class(Class::Interactive)).map_err(refused)?;
        anyhow::ensure!(again.bytes().as_ptr() == pointer, "the spare was reused");
        anyhow::ensure!(budget.held() == 0, "taken out of the pool");
        budget.release(again);
        // A Bulk frame's buffer, past the bound: given back to the allocator, never kept.
        let bulk = budget.reserve(4 << 20, Lane::Class(Class::Bulk)).map_err(refused)?;
        budget.release(bulk);
        anyhow::ensure!(budget.held() <= 64 << 10, "{}", budget.held());
        // Many given back at once: kept only while their bytes hold them.
        let mut held = Vec::new();
        for _ in 0..100 {
            held.push(budget.reserve(4096, Lane::Class(Class::Interactive)).map_err(refused)?);
        }
        for r in held {
            budget.release(r);
        }
        anyhow::ensure!(budget.held() <= 64 << 10, "{}", budget.held());
        anyhow::ensure!(budget.spare.len() <= 16, "{} spares of 4 KiB in 64 KiB", budget.spare.len());
        anyhow::ensure!(budget.used() == 0);
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: the budget refuses past its capacity as `Fixed` does, and a window's
    /// reservation carries no buffer.
    #[test]
    fn g11_s5_spares_refuse_past_their_capacity() -> anyhow::Result<()> {
        let mut budget = Spares::new(100, 1);
        let first = budget.reserve(60, Lane::Class(Class::Interactive)).map_err(refused)?;
        anyhow::ensure!(budget.reserve(41, Lane::Class(Class::Interactive)).err() == Some(Refusal::Budget));
        anyhow::ensure!((budget.used(), budget.refused()) == (60, 1));
        budget.release(first);
        anyhow::ensure!(budget.used() == 0);
        let window = budget.reserve(60, Lane::Window).map_err(refused)?;
        anyhow::ensure!(window.granted() == 60 && window.room() == 60 && window.bytes().is_empty());
        Ok(())
    }
}
