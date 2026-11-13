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
//! Its shell's waits on the keeper's answers follow the keeper's liveness and progress alone
//! ([`Waits`]): a wait ends, typed, when the link dies or resets, or when the keeper, alive, has
//! said nothing for a whole window of the slowest answer it has given.
//!
//! Every amount and count here is the keeper's or the runner's own: nothing is a number of this
//! module's.

use std::collections::VecDeque;

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
    /// This runner's reservations among `held` for the runs its ledger holds: what the keeper
    /// granted their starts that it hasn't released (`place::holder`). An earlier process's runs
    /// that outlived it, and releases not yet answered, count as everyone else's, so the view never
    /// says more room than the keeper has.
    pub own: Amounts,
    /// The most claims asking something this runner may hold live there at once, each one's room
    /// reserved through the keeper as one reservation: the most its holder may hold (Hex's
    /// `most_held`, the wire's message bound over the widest reservation, so that one sync lists
    /// them all: CV8, CV9), less those it holds for no live claim here (an earlier process's runs
    /// that outlived it, releases not yet answered). Its shell's count, said again as it moves: a
    /// claim past it fits nowhere, since its reserve would be refused. A run that asks nothing
    /// reserves nothing (Hex's holder takes no empty amount; what it uses is in the keeper's
    /// measured room, CV12) and isn't counted: it starts beside them, its cold start until kiln has
    /// learned what it asks.
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

/// Whether a claim of `amounts` reserves its room through the keeper: one that asks nothing
/// doesn't, as Hex's holder and keeper take no empty amount (refused `Empty`). It starts without a
/// reservation: what it uses is in the keeper's measured room (CV12) and CPU is shared, its cold
/// start until kiln has learned what it asks (agreed with Hex's lead, 2026-11-13).
pub fn reserves(amounts: &Amounts) -> bool {
    amounts.iter().any(|&a| a > 0)
}

/// Why the runner holds no synced session with its machine's keeper (the holder contract's items 5
/// and 6): each a state its shell names from what the keeper or its link said, or from its waits
/// ([`Waits`]).
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
    /// The keeper, its link alive, has said nothing (no answer, no hint) for a whole window of the
    /// slowest answer it has given ([`Waits`]): nothing new is admitted there until it answers
    /// again.
    Stalled,
}

impl Unsynced {
    /// Why, in words, as its waiting runs say it.
    pub fn why(self) -> &'static str {
        match self {
            Unsynced::Joining => "its machine's capacity keeper hasn't synced it yet (joining, or its link is down)",
            Unsynced::NotServed => "its machine's capacity keeper doesn't serve it (not among the holders the keeper's configuration names, or as many are served as it may): the person's to fix",
            Unsynced::Superseded => "a newer process of this runner holds its place with its machine's capacity keeper",
            Unsynced::Malformed => "its machine's capacity keeper closed its session for a word it couldn't take",
            Unsynced::Stalled => "its machine's capacity keeper, its link alive, has said nothing for longer than the slowest answer it has given",
        }
    }
}

/// What ended a wait on the keeper's answer, typed ([`Waits`]): the keeper's liveness and
/// progress, never a time of the holder's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitEnded {
    /// The link to the keeper ended: its transport found the keeper gone, by the transport's own
    /// liveness rule (QUIC's idle timeout, at least three probe timeouts of the measured round
    /// trip: RFC 9000 §10.1, RFC 9002 §6.2), or the connection failed.
    Died,
    /// The keeper started again, or reset the link (QUIC's stateless reset, RFC 9000 §10.3; its
    /// answer `Closed { Session }`): no word said on it is answered.
    Reset,
    /// The keeper, its link alive, has said nothing (no answer, no hint) for a whole window of the
    /// slowest answer it has given ([`Waits::slowest`]).
    Stalled,
}

/// A holder's waits on its keeper's answers (docs/lb.md "A machine's keeper"; Hex's holder wait
/// contract, decision 9, as its lead restated it on 2026-11-13). Its words go on one link and the
/// keeper answers them in the order said (CV9), so the answer due is the oldest word's. A wait
/// follows the keeper's liveness and progress alone: no time is seeded from records, and none is
/// multiplied (no constants: Shards' R03, as the user restated it on 2026-11-13). It ends, typed
/// ([`WaitEnded`]), when the link dies or resets, or when the keeper, its link alive, has said
/// nothing for a whole window of the slowest answer it has given in this process: the window runs
/// from the word waiting, or from the keeper's last word on the link (an answer, or a hint, which
/// answers nothing but shows it going on) where that came later. Each answer is timed from its
/// word, or from the answer before it where that came later (the word waited behind it, the keeper
/// answering in order); a hint times nothing. Until its first answer there is no such bound: a cold
/// start waits on liveness alone, and admits nothing new there meanwhile (CV11). A word whose wait
/// ended stays in its place, so an answer that comes late is matched to its own word, and makes the
/// slowest answer what it took. `now` is the shell's monotonic clock, in milliseconds.
#[derive(Debug, Clone, Default)]
pub struct Waits {
    /// When each word said on the link and not yet answered was said, oldest first.
    said: VecDeque<i64>,
    /// When the keeper last answered on the link.
    answered: Option<i64>,
    /// When the keeper last said anything on the link, an answer or a hint.
    heard: Option<i64>,
    /// The slowest answer the keeper has given, on any link of this process.
    slowest: Option<u64>,
}

impl Waits {
    /// A word said on the link at `now`, its answer due after those of the words before it.
    pub fn said(&mut self, now: i64) {
        self.said.push_back(now);
    }

    /// The keeper said something on the link at `now` (an answer, or a hint): it goes on, so the
    /// oldest word's window runs from now. It times no answer.
    pub fn heard(&mut self, now: i64) {
        self.heard = Some(self.heard.map_or(now, |at| at.max(now)));
    }

    /// The oldest word's answer came at `now`: how long it took, the slowest answer now at least
    /// that. None where no word waited (an answer the link owed nothing for).
    pub fn answered(&mut self, now: i64) -> Option<u64> {
        self.heard(now);
        let said = self.said.pop_front()?;
        let from = self.answered.map_or(said, |at| at.max(said));
        let took = u64::try_from(now.saturating_sub(from)).unwrap_or(0);
        self.slowest = Some(self.slowest.map_or(took, |s| s.max(took)));
        self.answered = Some(now);
        Some(took)
    }

    /// The link ended (`Died`) or reset (`Reset`): no word said on it will be answered, and the
    /// next link keeps its own order. How many were waiting. The slowest answer stays: it is the
    /// keeper's, not the link's.
    pub fn ended(&mut self) -> usize {
        let waiting = self.said.len();
        self.said.clear();
        self.answered = None;
        self.heard = None;
        waiting
    }

    /// When the oldest word's wait ends as stalled: once a whole window of the slowest answer has
    /// passed with nothing from the keeper, from its word or the keeper's last word after it. None
    /// while no word waits, or before the keeper's first answer.
    pub fn due(&self) -> Option<i64> {
        let said = *self.said.front()?;
        let slowest = i64::try_from(self.slowest?).unwrap_or(i64::MAX);
        let from = self.heard.map_or(said, |at| at.max(said));
        Some(from.saturating_add(slowest).saturating_add(1))
    }

    /// Whether the keeper is stalled at `now` ([`WaitEnded::Stalled`]): its oldest word is due.
    pub fn stalled(&self, now: i64) -> bool {
        self.due().is_some_and(|due| now >= due)
    }

    /// The slowest answer the keeper has given in this process, none before its first.
    pub fn slowest(&self) -> Option<u64> {
        self.slowest
    }

    /// The slowest answer at least `took`: one the party gave on another wait of this process, the
    /// slowest answer it has given anywhere in it being the bound.
    pub fn slowest_at_least(&mut self, took: u64) {
        self.slowest = Some(self.slowest.map_or(took, |s| s.max(took)));
    }

    /// How many words wait for their answers.
    pub fn waiting(&self) -> usize {
        self.said.len()
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
    /// No answer came, its wait ended as [`Waits`] says (the holder core's `Event::Unanswered`):
    /// its link died, or the keeper reset it or closed its session for one it opens again
    /// (`Closed { Session | NotSynced | Scope }`), or the keeper stalled.
    Unanswered(WaitEnded),
    /// It asks GPUs, which the keeper grants no holder by count (`Refused { Devices }`, CV3: until
    /// device grants by identity). The ledger offers no such claim where a keeper is known.
    Devices,
    /// It asks nothing (`Refused { Empty }`). Its holder says no such reserve: a run that asks
    /// nothing starts without one.
    Empty,
    /// This process of its runner has numbered every reservation its epoch can (the holder core's
    /// `NotNow::Exhausted`): its next process's epoch numbers them anew.
    Exhausted,
    /// No reserve was said, or the keeper closed its session for good: the runner isn't synced
    /// with its machine's keeper (CV11), and why.
    Unsynced(Unsynced),
}

impl NotGranted {
    /// Why, in words, as the run's owner is told it.
    pub fn why(self) -> &'static str {
        match self {
            NotGranted::Room => "its machine's capacity keeper refused its reserve: no room for it there now",
            NotGranted::Full => "its machine's capacity keeper refused its reserve: this runner holds as many as the keeper keeps for it",
            NotGranted::Disagrees => "its machine's capacity keeper granted another amount than its reserve asked, given back",
            NotGranted::Unanswered(WaitEnded::Died) => "its reserve went unanswered: its link to its machine's capacity keeper ended",
            NotGranted::Unanswered(WaitEnded::Reset) => "its reserve went unanswered: its machine's capacity keeper started again",
            NotGranted::Unanswered(WaitEnded::Stalled) => "its reserve went unanswered: its machine's capacity keeper said nothing for longer than the slowest answer it has given",
            NotGranted::Devices => "its machine's capacity keeper refused its reserve: it asks GPUs, which the keeper grants no holder by count",
            NotGranted::Empty => "its machine's capacity keeper refused its reserve as asking nothing",
            NotGranted::Exhausted => "this runner's process has numbered every reservation it can: its next process numbers them anew",
            NotGranted::Unsynced(why) => why.why(),
        }
    }
}
