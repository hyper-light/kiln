//! What owners and runners say to each other (docs/lb.md "Messages"): small `Copy` values, so a
//! decision copies a few words and allocates nothing. Every word a runner sends carries its room
//! ([`Word`]), so an owner's view of it is its last word less what the owner has sent it since.
//! The shell carries them over the runner's link, in order on each link; a link that drops loses
//! what was on it, and both ends are told ([`super::Owner::link_down`], [`super::Runner::link_down`]).

use super::slab::Handle;
use super::{Amounts, OwnerId, RunnerId};

/// A runner's room as it says it, with every message it sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Word {
    /// The runner's epoch: it grows each time the runner starts again, and fences what was said
    /// to the runner before (a start sent under an older epoch is declined).
    pub epoch: u32,
    /// All of its memory, disk and GPUs are free for a new run (as its budget has them), so an
    /// owner need not read `rest`.
    pub whole: bool,
    /// CPU free for a new run, in thousandths: free CPU less what the first run in its line keeps.
    pub cpu: u64,
    /// The same of memory, disk and GPUs.
    pub rest: [u64; 3],
}

impl Word {
    /// Its room as amounts.
    pub fn room(&self) -> Amounts {
        let [m, d, g] = self.rest;
        [self.cpu, m, d, g]
    }
}

/// A run an owner sends a runner, to start now or to wait in its line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Start {
    pub owner: OwnerId,
    /// The owner's handle for it: the runner says it back in each word about it.
    pub run: Handle,
    /// The runner's epoch the owner decided under.
    pub epoch: u32,
    pub need: Amounts,
    /// The last of the runner's Ended the owner has heard (their `seq`): those, the runner may
    /// forget.
    pub ack: u32,
}

/// An owner's word to a runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToRunner {
    /// Start it now if it fits, or say why not (Declined) with your room.
    Start(Start),
    /// Keep it in your line, and say Ready once you hold its room.
    Wait(Start),
    /// The room you hold for it (`at`, your Ready's) is mine: start it.
    Go { owner: OwnerId, run: Handle, at: Handle, epoch: u32 },
    /// Forget it (`at`): it went elsewhere or is gone. Room held for it is free again.
    Withdraw { owner: OwnerId, run: Handle, at: Handle },
}

/// Why a runner didn't take a start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// Its room is less than the need now (the word says how much there is).
    Room,
    /// It was sent under an epoch that isn't the runner's.
    Epoch,
    /// The need is more than the runner's whole budget.
    Never,
    /// What it refers to isn't here (a Go for room no longer held).
    Gone,
}

/// Which of an owner's words a runner declined: a run can have an old Wait and a newer Start in
/// flight to the same runner at once (it was placed again as the runner started over), and each
/// decline must reach the one it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Of {
    Start,
    Wait,
    Go,
}

/// A runner's word to an owner. Each carries the runner's [`Word`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToOwner {
    /// It started here (`at`: its handle here). Said again for each of the owner's live runs
    /// when their link comes back, before Synced.
    Started { runner: RunnerId, run: Handle, at: Handle, word: Word },
    /// It didn't start (or wait, or go), and why.
    Declined { runner: RunnerId, run: Handle, word: Word, why: Why, of: Of },
    /// It waits in line here (`at`).
    Queued { runner: RunnerId, run: Handle, at: Handle, word: Word },
    /// Its room is held here (`at`) until Go or Withdraw.
    Ready { runner: RunnerId, run: Handle, at: Handle, word: Word },
    /// It ended here: the `seq`-th run to end here of this owner's. The runner says it again when
    /// their link comes back until the owner acknowledges it (a Start's `ack`), so a run that ends
    /// while its link is down is never taken for one that never started.
    Ended { runner: RunnerId, run: Handle, word: Word, seq: u32 },
    /// The runner's room changed (sent to the owners that watch it; one waiting to go is
    /// replaced by a newer one).
    Hint { runner: RunnerId, word: Word },
    /// The link is back and every live run of the owner's here has been said again (Started):
    /// any other the owner had here isn't.
    Synced { runner: RunnerId, word: Word },
}

impl ToOwner {
    /// The runner it comes from.
    pub fn runner(&self) -> RunnerId {
        match *self {
            ToOwner::Started { runner, .. }
            | ToOwner::Declined { runner, .. }
            | ToOwner::Queued { runner, .. }
            | ToOwner::Ready { runner, .. }
            | ToOwner::Ended { runner, .. }
            | ToOwner::Hint { runner, .. }
            | ToOwner::Synced { runner, .. } => runner,
        }
    }

    /// Its word.
    pub fn word(&self) -> Word {
        match *self {
            ToOwner::Started { word, .. }
            | ToOwner::Declined { word, .. }
            | ToOwner::Queued { word, .. }
            | ToOwner::Ready { word, .. }
            | ToOwner::Ended { word, .. }
            | ToOwner::Hint { word, .. }
            | ToOwner::Synced { word, .. } => word,
        }
    }
}

impl ToRunner {
    /// The owner it comes from.
    pub fn owner(&self) -> OwnerId {
        match *self {
            ToRunner::Start(s) | ToRunner::Wait(s) => s.owner,
            ToRunner::Go { owner, .. } | ToRunner::Withdraw { owner, .. } => owner,
        }
    }
}
