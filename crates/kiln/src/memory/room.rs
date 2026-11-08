//! What the server may hold of what runners send it on its way to their sessions' copies, and the bounds of the buffers
//! that hold it, each a share of that (join-burst, item 1). They were each a fixed window, 64 MiB (`crate::net::WINDOW`):
//! on a 128 to 256 MB instance each was half its memory or more, and a burst of runners joining filled them together.
//!
//! - **The room**: what the server may hold, its cgroups' `memory.max` or the machine's memory where none is set (and its
//!   rlimits: kiln-rt's effective capacity, `hyper_rt::machine::facts`), less what it holds now beside these buffers: its
//!   resident memory (what the kernel charges it: `kiln_memory_resident_bytes`, every kind) less what they hold
//!   (`kiln_memory_bytes` of [`IN_FLIGHT`]'s owners).
//! - **A share**: the room cut into one for each runner connected, as much as its connection's receive window lets it
//!   send unread (QUIC's flow control, RFC 9000 §4.1), and one for each buffer runners' bytes wait in on the server, which
//!   fill together while the copies are behind ([`buffers`]). So all of them full never pass the room, and past them
//!   QUIC's flow control holds the runners: what they send waits with them, where it's kept already, not in the server.
//! - **Measured again** as a runner comes or goes (the net thread) and as the kernel says memory is short
//!   (`crate::os::pressure`), never on a timer. Each queue sized by it ([`follow`]) takes the new share then, and whoever
//!   waits for its room looks again where it grew.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock, Weak};

use super::Owner;
use super::queue::Room;

/// The owners of runners' bytes on their way to the copies, in the buffers the room is for: left out of what the server
/// holds beside them.
const IN_FLIGHT: [&Owner; 3] = [&super::owners::NET_EVENTS, &super::owners::SESSION_INBOXES, &super::owners::SESSION_PACKING];

/// The server's buffers of runners' bytes, which fill together while its copies are behind, each a share: the net
/// thread's own (what it holds before it holds the runners' lanes), the hub loops' queues (`net_events`), each of
/// `shards` shards' session inbox (`session_inboxes`), and the copies' sealed texts waiting for the packer
/// (`session_packing`).
pub const fn buffers(shards: usize) -> usize {
    shards.saturating_add(3)
}

/// A share of `room` bytes, `runners` runners connected to a server of `shards` shards: the room cut into a window for
/// each runner (one at the least: the next to come) and a share for each of its buffers ([`buffers`]).
pub fn share_of(room: u64, runners: usize, shards: usize) -> u64 {
    let cuts = runners.max(1).saturating_add(buffers(shards));
    room.checked_div(u64::try_from(cuts).unwrap_or(u64::MAX)).unwrap_or(0)
}

/// What the server may hold, in bytes: the least of the machine's memory and what its cgroups and rlimits bound it to.
/// Asked once: the OS's answer doesn't change while it runs.
fn limit() -> u64 {
    static LIMIT: OnceLock<u64> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        let facts = hyper_rt::machine::facts::Facts::query();
        hyper_rt::machine::facts::effective_capacity(facts.memory.total, facts.memory.limit)
    })
}

/// What the process holds now, in bytes, as the kernel charges it: its resident memory (Linux: `/proc/self/statm`; macOS:
/// its task's resident size). None where the OS says nothing (Windows, for now): the room is then the whole of the limit.
fn resident() -> u64 {
    hyper_rt::machine::facts::Facts::resident_now().unwrap_or(0)
}

/// The share and what it's measured from, and the queues it sizes.
pub struct Shares {
    runners: AtomicUsize,
    /// The share last measured; none until first asked.
    share: OnceLock<AtomicUsize>,
    followers: OnceLock<crate::rcu::Rcu<Vec<Weak<Room>>>>,
}

impl Shares {
    pub const fn new() -> Self {
        Self { runners: AtomicUsize::new(0), share: OnceLock::new(), followers: OnceLock::new() }
    }

    /// The share now, in bytes: measured as first asked.
    pub fn share(&self) -> usize {
        match self.share.get() {
            Some(s) => s.load(Ordering::Relaxed),
            None => self.measure(),
        }
    }

    /// `n` runners are connected now: the share measured again for them.
    pub fn runners(&self, n: usize) -> usize {
        self.runners.store(n, Ordering::Relaxed);
        self.measure()
    }

    /// `room` sized by the share from now on: now, and each time it's measured again. One that goes is let go of.
    pub fn follow(&self, room: &Arc<Room>) {
        room.resize(self.share());
        self.followers().update(|f| {
            f.retain(|w| w.strong_count() > 0);
            f.push(Arc::downgrade(room));
        });
    }

    /// The share measured again, from what the server may hold and what it holds now: the share, and each queue that
    /// follows it ([`Shares::follow`]) resized to it.
    pub fn measure(&self) -> usize {
        let in_flight = IN_FLIGHT.iter().map(|o| u64::try_from(o.bytes()).unwrap_or(u64::MAX)).fold(0, u64::saturating_add);
        self.measure_with(limit(), resident(), in_flight)
    }

    /// [`Shares::measure`] of `limit` bytes the server may hold, `resident` held now, `in_flight` of it in the buffers.
    fn measure_with(&self, limit: u64, resident: u64, in_flight: u64) -> usize {
        let room = limit.saturating_sub(resident.saturating_sub(in_flight));
        let share = usize::try_from(share_of(room, self.runners.load(Ordering::Relaxed), crate::wire::fan::shards())).unwrap_or(usize::MAX);
        self.share.get_or_init(|| AtomicUsize::new(share)).store(share, Ordering::Relaxed);
        for room in self.followers().load().iter().filter_map(Weak::upgrade) {
            room.resize(share);
        }
        share
    }

    fn followers(&self) -> &crate::rcu::Rcu<Vec<Weak<Room>>> {
        self.followers.get_or_init(crate::rcu::Rcu::default)
    }
}

impl Default for Shares {
    fn default() -> Self {
        Self::new()
    }
}

/// The server's.
static SHARES: Shares = Shares::new();

/// The server's share now ([`Shares::share`]).
pub fn share() -> usize {
    SHARES.share()
}

/// `n` runners connected to the server now ([`Shares::runners`]).
pub fn runners(n: usize) -> usize {
    SHARES.runners(n)
}

/// One of the server's queues sized by its share ([`Shares::follow`]).
pub fn follow(room: &Arc<Room>) {
    SHARES.follow(room);
}

/// The server's share measured again ([`Shares::measure`]): the kernel said memory is short.
pub fn measure() -> usize {
    SHARES.measure()
}

#[cfg(test)]
mod tests {
    use super::*;

    const MIB: u64 = 1 << 20;

    /// join-burst, items 1 and 2: the room is cut into a window for each runner connected (one at the least) and a share for
    /// each buffer that fills with theirs while the copies are behind (the net thread's, the hub loops' queues, each shard's
    /// inbox, the copies' texts waiting for the packer), so together, full, they hold the room and no more. A 256 MiB
    /// instance holding 112 MiB beside them, 4 runners, 2 shards: 144 MiB in 9 shares of 16 MiB, where each was a 64 MiB
    /// window (4 x 64 MiB of windows, 64 MiB the net thread held before it held the lanes, 64 MiB of the loops' queues, 64
    /// MiB of each inbox: 512 MiB in all) and the texts waiting for the packer had no bound.
    #[test]
    fn join_burst_the_room_is_cut_into_a_share_for_each_runner_and_buffer() {
        assert_eq!(buffers(2), 5);
        assert_eq!(share_of(144 * MIB, 4, 2), 16 * MIB);
        let full = share_of(144 * MIB, 4, 2) * u64::try_from(4 + buffers(2)).unwrap_or(0);
        assert_eq!(full, 144 * MIB, "full, they hold the room and no more");
        assert_eq!(share_of(144 * MIB, 0, 2), share_of(144 * MIB, 1, 2), "the next runner to come has a window");
        assert!(share_of(144 * MIB, 32, 2) < share_of(144 * MIB, 4, 2), "more runners, smaller shares");
        assert_eq!(share_of(0, 4, 2), 0, "no room: nothing past one item at a time");
        let old = 4 * 64 * MIB + 64 * MIB + 64 * MIB + 2 * 64 * MIB;
        assert!(full * 3 < old, "under a third of the 512 MiB the windows and buffers could hold together: {full} of {old}");
    }

    /// join-burst, item 1: the share is measured from what the server may hold less what it holds beside its buffers, and
    /// the queues that follow it take each new share: grown, a send that waited for room goes; shrunk, a send past it
    /// waits. What the buffers hold themselves isn't taken from the room (they'd shrink it as they filled).
    #[test]
    fn join_burst_queues_follow_the_share_as_its_measured_again() -> anyhow::Result<()> {
        static TEST_BUFFER: Owner = Owner::new("test_join_burst_buffer");
        let shares = Shares::new();
        let cuts = u64::try_from(1 + buffers(crate::wire::fan::shards()))?;
        let measured = shares.measure_with(256 * MIB, 96 * MIB, 0);
        assert_eq!(u64::try_from(measured)?, 160 * MIB / cuts);
        let (tx, rx) = super::super::queue::bounded::<Vec<u8>>(&TEST_BUFFER, 0);
        shares.follow(tx.room());
        assert_eq!(tx.room().bound(), measured, "the queue follows the share");
        // A send that half fills it, then one past it, refused; the share measured again with more room: it goes.
        let item = |n: usize| vec![0u8; n];
        tx.try_send(item(measured / 2)).map_err(|_| anyhow::anyhow!("half the share refused"))?;
        anyhow::ensure!(tx.try_send(item(measured)).is_err(), "past the share: refused");
        let grown = shares.measure_with(512 * MIB, 96 * MIB, 0);
        anyhow::ensure!(grown > measured && tx.room().bound() == grown, "grown: {measured} then {grown}");
        tx.try_send(item(measured)).map_err(|_| anyhow::anyhow!("the grown share refused what fits it"))?;
        // Shrunk: what's held stands; a send past the new share is refused until what's held is taken.
        let shrunk = shares.measure_with(128 * MIB, 96 * MIB, 0);
        anyhow::ensure!(shrunk < measured && tx.room().bound() == shrunk);
        anyhow::ensure!(tx.try_send(item(1)).is_err(), "past the shrunk share: refused");
        while rx.try_recv().is_ok() {}
        tx.try_send(item(1)).map_err(|_| anyhow::anyhow!("an empty queue refused"))?;
        // What the buffers hold isn't held beside them: the share doesn't shrink as they fill.
        assert_eq!(shares.measure_with(256 * MIB, 96 * MIB + 40 * MIB, 40 * MIB), measured);
        // A queue that went is let go of: the next follower finds only the live ones.
        drop((tx, rx));
        let (other, _keep) = super::super::queue::bounded::<Vec<u8>>(&TEST_BUFFER, 0);
        shares.follow(other.room());
        assert_eq!(shares.followers().load().len(), 1, "the dropped queue let go of");
        Ok(())
    }

    /// join-burst, item 1: a runner coming or going cuts the room again for the runners then connected.
    #[test]
    fn join_burst_runners_coming_and_going_cut_the_share_again() {
        let shares = Shares::new();
        let at = |n: usize| {
            shares.runners.store(n, Ordering::Relaxed);
            shares.measure_with(256 * MIB, 96 * MIB, 0)
        };
        let (one, four) = (at(1), at(4));
        assert!(four < one, "{one} then {four}");
        assert_eq!(at(0), one, "none connected: the next to come has a window");
        assert!(shares.runners(4) > 0, "a runner's coming measures the server's room again");
    }
}
