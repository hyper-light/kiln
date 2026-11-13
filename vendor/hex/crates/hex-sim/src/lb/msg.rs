//! What owners and nodes say to each other (kiln `crates/kiln-ci/src/lb/msg.rs` at f20858c9c):
//! small `Copy` values. Every word a node sends carries its room ([`Word`]), so an owner's view of
//! it is its last word less what the owner has sent it since. The shell carries them over the
//! node's link, in order on each link; a link that drops loses what was on it, and both ends are
//! told ([`super::Owner::link_down`], [`super::Runner::link_down`]).

use super::slab::Handle;
use super::{Amounts, OwnerId, RunnerId};

/// A node's room as it says it, with every message it sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Word {
    /// The node agent's epoch: it grows each time the agent starts again, and fences what was said
    /// to the agent before (a start sent under an older epoch is declined).
    pub epoch: u32,
    /// All of its memory, disk and GPUs are free for a new box (as its budget has them), so an
    /// owner need not read `rest`.
    pub whole: bool,
    /// CPU free for a new box, in thousandths: free CPU less what the first box in its line keeps.
    pub cpu: u64,
    /// The same of memory, disk and GPUs.
    pub rest: [u64; 3],
    /// Its per-core speed, measured, in 1/1024 of the reference core's (0: not measured).
    pub speed: u32,
    /// Its per-core slowness under the contention it measures now, in 1/1024 of uncontended (0:
    /// not measured).
    pub slowness: u32,
}

impl Word {
    /// Its room as amounts.
    pub fn room(&self) -> Amounts {
        let [m, d, g] = self.rest;
        [self.cpu, m, d, g]
    }
}

/// A box an owner sends a node, to create and start now or to wait in its line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Start {
    pub owner: OwnerId,
    /// The owner's handle for it: the node says it back in each word about it.
    pub run: Handle,
    /// The node's epoch the owner decided under.
    pub epoch: u32,
    pub need: Amounts,
}

/// An owner's word to a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToRunner {
    /// Create and start it now if it fits, or say why not (Declined) with your room.
    Start(Start),
    /// Keep it in your line, and say Ready once you hold its room.
    Wait(Start),
    /// The room you hold for it (`at`, your Ready's) is mine: create and start it.
    Go {
        owner: OwnerId,
        run: Handle,
        at: Handle,
        epoch: u32,
    },
    /// Forget it (`at`): it went elsewhere or was deleted. Room held for it is free again.
    Withdraw {
        owner: OwnerId,
        run: Handle,
        at: Handle,
    },
}

/// Why a node didn't take a start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Why {
    /// Its room is less than the need now (the word says how much there is).
    Room,
    /// It was sent under an epoch that isn't the node's.
    Epoch,
    /// The need is more than the node's whole budget.
    Never,
    /// What it refers to isn't here (a Go for room no longer held).
    Gone,
}

/// Which of an owner's words a node declined: a box can have an old Wait and a newer Start in
/// flight to the same node at once, and each decline must reach the one it answers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Of {
    Start,
    Wait,
    Go,
}

/// A node's word to an owner. Each carries the node's [`Word`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToOwner {
    /// It was created and started here (`at`: its handle here). Said again for each of the
    /// owner's live boxes when their link comes back, before Synced.
    Started {
        runner: RunnerId,
        run: Handle,
        at: Handle,
        word: Word,
    },
    /// It didn't start (or wait, or go), and why.
    Declined {
        runner: RunnerId,
        run: Handle,
        word: Word,
        why: Why,
        of: Of,
    },
    /// It waits in line here (`at`).
    Queued {
        runner: RunnerId,
        run: Handle,
        at: Handle,
        word: Word,
    },
    /// Its room is held here (`at`) until Go or Withdraw.
    Ready {
        runner: RunnerId,
        run: Handle,
        at: Handle,
        word: Word,
    },
    /// It was deleted here (or exited), and its room is free.
    Ended {
        runner: RunnerId,
        run: Handle,
        word: Word,
    },
    /// The node's room changed (sent to the owners that watch it; one waiting to go is replaced
    /// by a newer one).
    Hint { runner: RunnerId, word: Word },
    /// The link is back and every live box of the owner's here has been said again (Started):
    /// any other the owner had here isn't. `ended`: how many of the owner's boxes have ended here,
    /// so the owner knows how many ends it missed, and never takes a box that ran and was deleted
    /// while the link was down for one that never started.
    Synced {
        runner: RunnerId,
        word: Word,
        ended: u32,
    },
}

impl ToOwner {
    /// The node it comes from.
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
