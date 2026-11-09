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
    /// Its per-core speed, measured, in 1/1024 of the reference core's (0: not measured): wide
    /// asks rank runners by free cores times it, core-equivalents (docs/lb.md "Rank").
    pub speed: u32,
    /// Its per-core slowness under the contention it measures now, in 1/1024 of uncontended (0:
    /// not measured): narrow asks rank by speed over it (docs/lb.md "Rank").
    pub slowness: u32,
    /// Its disk's effective rate under its load now, bytes a second (0: not measured): an ask's
    /// I/O term (docs/lb.md "Per-ask terms").
    pub disk: u64,
    /// Its own network's rate, bytes a second (0: not measured): an ask's network term.
    pub net: u64,
    /// How many times what it measured of itself changed (its speed, slowness, disk or network;
    /// [`super::Runner::measure`]), past [`NO_FAST`] going round to 1: a word's stamp carries it, so
    /// an owner's fast paths take a word only while nothing measured changed since it read one.
    pub measured: u32,
    /// Under room tokens ([`super::tokens`]): another owner's run waited here for room since this
    /// owner last heard, so a token this owner can't use is given back to be forwarded to it
    /// ([`ToRunner::Release`]). Always false under hints.
    pub wants: bool,
}

impl Word {
    /// Its room as amounts.
    pub fn room(&self) -> Amounts {
        let [m, d, g] = self.rest;
        [self.cpu, m, d, g]
    }

    /// What the owner's fast paths read of it.
    #[inline(always)]
    pub fn brief(&self) -> Brief {
        Brief { stamp: stamp(self.epoch, self.whole, self.measured), cpu: super::narrow(self.cpu) }
    }
}

/// What a word's stamp has in its high half when its runner's memory, disk or GPUs aren't all
/// free: an owner's stamp never has it, so such a word never takes the fast path.
pub(crate) const NOT_WHOLE: u32 = u32::MAX;

/// What an owner's stamp for a runner has in its high half when it doesn't take words on the
/// fast path (not known, link down, memory, disk or GPUs not all free): no word's stamp has it.
pub(crate) const NO_FAST: u32 = u32::MAX - 1;

/// The high half of a word's stamp: its measure count ([`Word::measured`]) when all of its
/// memory, disk and GPUs are free (a count past what a stamp can say goes the general path, as a
/// word not whole does).
#[inline(always)]
pub(crate) const fn tag(whole: bool, measured: u32) -> u32 {
    if whole && measured < NO_FAST { measured } else { NOT_WHOLE }
}

/// A word's stamp: the epoch in the low half, [`tag`] in the high half. One comparison with what
/// its owner keeps says the word is of the kind the fast paths take: the epoch it knows, all of
/// the runner's memory, disk and GPUs free as before, nothing measured changed.
#[inline(always)]
pub(crate) const fn stamp(epoch: u32, whole: bool, measured: u32) -> u64 {
    epoch as u64 | (tag(whole, measured) as u64) << 32
}

/// What the fast paths read of a [`Word`], so a word that is only to be read there isn't made up
/// whole: its [`stamp`] and its CPU free for a new run (a view: what passes 2^32 - 1 is taken as
/// that).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Brief {
    pub stamp: u64,
    pub cpu: u32,
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
    /// Under room tokens: the owner can't use `amount` of its token here (no run of its waits
    /// for this runner), and gives it back to be forwarded once, to the owner whose run last
    /// waited here for room ([`super::tokens`]). Appended: the wire only grows.
    Release { owner: OwnerId, epoch: u32, amount: Amounts },
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
    /// It ended here.
    Ended { runner: RunnerId, run: Handle, word: Word },
    /// The runner's room changed (sent to the owners that watch it; one waiting to go is
    /// replaced by a newer one).
    Hint { runner: RunnerId, word: Word },
    /// The link is back and every live run of the owner's here has been said again (Started):
    /// any other the owner had here isn't. `ended`: how many of the owner's runs have ended here
    /// this epoch, so the owner knows how many ends it missed, and never takes a run that ran and
    /// ended while the link was down for one that never started.
    Synced { runner: RunnerId, word: Word, ended: u32 },
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

impl ToOwner {
    /// Its word, to be told per owner (under room tokens each owner hears what it holds).
    pub(crate) fn word_mut(&mut self) -> &mut Word {
        match self {
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
            ToRunner::Go { owner, .. } | ToRunner::Withdraw { owner, .. } | ToRunner::Release { owner, .. } => owner,
        }
    }
}
