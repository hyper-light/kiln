//! A machine's capacity keeper, as the runner's ledger takes it (docs/lb.md "A machine's keeper";
//! Hex's `docs/design/capacity-view.md` CV1, CV3, CV7, CV8, CV10, CV11; kiln plan
//! 01a1cd8e862e7169a074db3d5fa33f3d, step 13). Where Hex's node keeps a machine's room, Hex's boxes
//! and kiln's runs admit against that one ledger: kiln's runner holds room there by reserving it
//! through the keeper, its run starting only on the keeper's grant (its shell's). This is the ledger's
//! side: what the runner may admit, given where it stands with the keeper.
//!
//! - **No keeper** ([`Keeper::None`]): it admits on its own measured room, as ever (CV10). A keeper
//!   is known only where the runner's configuration names one: off until declared.
//! - **Unsynced** ([`Keeper::Unsynced`]): a keeper is known and the runner holds no synced session
//!   with it, so it admits nothing new there (CV11; the holder contract's item 6). Its runs waiting
//!   say why ([`super::msg::Reason::Unsynced`]), its kernel is cordoned so its words say no room and
//!   owners place elsewhere, and what runs goes on, held in the keeper's ledger. A keeper that stops
//!   answering is never taken for none: that would double-book what it still holds (CV10).
//! - **Synced** ([`Keeper::Synced`]): what it admits is held to the room the keeper's last word leaves
//!   it ([`View::room_for_holder`]): the machine's room less what everyone else holds, the keeper's
//!   own rule (CV3: a charge fits iff `c <= room - local - foreign` in every dimension). Its CPUs are
//!   never overcommitted past that room, which the keeper grants none past, it holds no GPUs (CV3:
//!   a holder's reserve asking any is refused, typed, until device grants by identity), and it holds
//!   no more claims than its holder may reserve ([`View::most_claims`], CV8). The keeper still
//!   validates each reserve: this is the decider's copy (CV7; Omega, Schwarzkopf et al., EuroSys
//!   2013, §3.4), kept current by every answer and hint, so a refusal ([`NotGranted`]) costs a
//!   placement again, never a double booking.
//!
//! Every amount and count here is the keeper's or the runner's own: nothing is a number of this
//! module's.

use super::ask::{self, Amounts};

/// The keeper's view of its machine as its last answer or hint said it (CV7), and what of it is this
/// runner's own.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct View {
    /// The machine's room, as the keeper's host measured it.
    pub room: Amounts,
    /// Everything held there: the host's own admissions and every holder's reservations, this
    /// runner's among them.
    pub held: Amounts,
    /// This runner's reservations among `held`: what the keeper granted it that it hasn't released,
    /// its holder's own count (which each sync holds to the keeper's, CV5).
    pub own: Amounts,
    /// The most claims this runner may hold live there at once, each claim's room reserved through
    /// the keeper as one reservation: the most its holder may hold (Hex's `most_held`, the wire's
    /// message bound over the widest reservation, so that one sync lists them all: CV8, CV9), less
    /// those it holds for no live claim here (an earlier process's runs that outlived it, releases
    /// not yet answered). Its shell's count, said again as it moves: a claim past it fits nowhere,
    /// since its reserve would be refused for it.
    pub most_claims: u64,
}

impl View {
    /// What everyone else holds there: the host's own admissions and the other holders'
    /// reservations. None in a dimension where its own pass `held`, which only a count gone stale
    /// says: the keeper's next word puts it right.
    pub fn foreign(&self) -> Amounts {
        ask::less(&self.held, &self.own)
    }

    /// The room this runner may admit against there (CV3): the machine's room less what everyone
    /// else holds, none in a dimension they hold past it (the view is over: nothing new until it
    /// fits), and no GPUs, which the keeper grants no holder by count.
    pub fn room_for_holder(&self) -> Amounts {
        let [cpu, mem, disk, _gpus] = ask::less(&self.room, &self.foreign());
        [cpu, mem, disk, 0]
    }
}

/// Why the runner holds no synced session with its machine's keeper (the holder contract's items 5
/// and 6): each a state its shell names from what the keeper, or its link, said.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsynced {
    /// Its hello or sync is said and not answered yet, or its link is down and it says hello again
    /// (the keeper restarted, the link dropped, `Closed { Session | NotSynced }`).
    Joining,
    /// The keeper doesn't serve it: `Closed { HoldersFull | NotAuthorized }`, or its handshake
    /// refused for an identity the keeper's configuration doesn't name. The person's to fix.
    NotServed,
    /// A newer process of this runner is the keeper's holder (`Closed { OlderEpoch }`): this one
    /// admits nothing more there.
    Superseded,
    /// The keeper closed its session for a word it couldn't take (`Closed { Invalid }`): the
    /// runner's own defect.
    Malformed,
}

impl Unsynced {
    /// Why, in words, as its waiting runs say it.
    pub fn why(self) -> &'static str {
        match self {
            Unsynced::Joining => "its machine's capacity keeper hasn't synced it yet (joining, or its link is down)",
            Unsynced::NotServed => "its machine's capacity keeper doesn't serve it (not among the holders the keeper's configuration names, or as many are served as it may): the person's to fix",
            Unsynced::Superseded => "a newer process of this runner holds its place with its machine's capacity keeper",
            Unsynced::Malformed => "its machine's capacity keeper closed its session for a word it couldn't take",
        }
    }
}

/// Where the runner stands with its machine's keeper: its shell says each change
/// ([`super::runner::Admission::keeper_said`]).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Keeper {
    /// None is known: it admits on its own room.
    #[default]
    None,
    /// One is known and no synced session holds: nothing new is admitted, and why.
    Unsynced(Unsynced),
    /// Synced: the keeper's view as its last word said it.
    Synced(View),
}

impl Keeper {
    /// Whether a keeper is known for the machine.
    pub fn known(&self) -> bool {
        !matches!(self, Keeper::None)
    }

    /// Why nothing new is admitted: a keeper known and not synced.
    pub fn unsynced(&self) -> Option<Unsynced> {
        match self {
            Keeper::Unsynced(why) => Some(*why),
            Keeper::None | Keeper::Synced(_) => None,
        }
    }

    /// The room the keeper's view leaves this runner, while synced.
    pub fn room(&self) -> Option<Amounts> {
        match self {
            Keeper::Synced(view) => Some(view.room_for_holder()),
            Keeper::None | Keeper::Unsynced(_) => None,
        }
    }

    /// The most claims it may hold live there, while synced ([`View::most_claims`]).
    pub fn most_claims(&self) -> Option<u64> {
        match self {
            Keeper::Synced(view) => Some(view.most_claims),
            Keeper::None | Keeper::Unsynced(_) => None,
        }
    }
}

/// Why a run's reserve through its machine's keeper wasn't granted, as its shell heard it (the
/// holder contract's items 4 and 5): the run never starts under that claim. Its shell reserves a
/// claim's room before the ledger starts it (as Hex's stand-in holder does: the run is written down
/// on its grant, before its work starts), and says each refusal's view first, so the run then
/// waits on the room and claims there are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotGranted {
    /// It doesn't fit the room the keeper has left (`Refused { Room }`).
    Room,
    /// Its holder holds as many reservations as the keeper keeps for one (`Refused { Full }`, or
    /// the holder core's own `NotNow::Full`).
    Full,
    /// The keeper granted another amount than its reserve asked, which its holder gave back
    /// (`Disagrees`).
    Disagrees,
    /// No answer came, or none could be asked: its link dropped, its wait ran out (the holder
    /// core's `Event::Unanswered`), or its holder wasn't synced as it asked (`NotNow::Down`).
    Unanswered,
}

impl NotGranted {
    /// Why, in words, as the run's owner is told it.
    pub fn why(self) -> &'static str {
        match self {
            NotGranted::Room => "its machine's capacity keeper refused its reserve: no room for it there now",
            NotGranted::Full => "its machine's capacity keeper refused its reserve: this runner holds as many as the keeper keeps for it",
            NotGranted::Disagrees => "its machine's capacity keeper granted another amount than its reserve asked, given back",
            NotGranted::Unanswered => "its reserve with its machine's capacity keeper went unanswered (its link dropped, or its wait ran out)",
        }
    }
}
