//! kiln's runner as the holder of its machine's capacity keeper (docs/lb.md "A machine's keeper";
//! Hex's `docs/design/capacity-view.md`, "What kiln's runner implements as a holder on
//! `CAPACITY_PORT`"; kiln plan 01a1cd8e862e7169a074db3d5fa33f3d, step 13). Hex's holder core,
//! `hex_api::capacity::Holder`, keeps the order of words (Shards' R12: one implementation); a
//! [`Holding`] drives it for the runner's ledger, with the holder's waits ([`Waits`]) and the
//! starts its reservations are for:
//!
//! - where the runner stands with its keeper, said as it changes ([`Event::Keeper`], for
//!   `Admission::keeper_said`): synced, the keeper's view with the runner's own share of it and the
//!   claims it may hold; else why not (CV11);
//! - each start's reserve ([`Holding::reserve`]), granted or not ([`Event::Granted`],
//!   [`Event::NotGranted`]): its shell starts the run on its grant alone (`Admission::start`, or
//!   `Admission::fast` for a kernel start), and a start not granted lapses its claim or declines
//!   its kernel start (`Admission::not_granted`, `Admission::fast_not_granted`);
//! - its room given back once its work can no longer run ([`Holding::release`]);
//! - when to wake it ([`Holding::due`]): a keeper that, its link alive, has said nothing (no
//!   answer, no hint) for a whole window of the slowest answer it has given is stalled, and each
//!   reserve waiting on it is not granted.
//!
//! Pure, as the rest of the place core: its shell carries each word to the keeper and each answer
//! back on one link, in order, says when a link opens and ends, and passes its monotonic clock in,
//! in milliseconds. Every number here is the keeper's, the wire's or the runner's own: the most it
//! may hold is the most reservations one sync lists within the wire's message bound
//! (`hex_core::capacity::most_held`, CV8, CV9).

use std::collections::BTreeSet;

use hex_api::capacity::{self as hex, Close, Held, ReservationId, ToHolder, ToKeeper, Why};

use super::ask::{self, Amounts};
use super::keeper::{self, Keeper, NotGranted, Unsynced, View, WaitEnded, Waits};

/// The holder core's link, as its words and answers say it (the holder's states: Hex's
/// capacity-view.md, "The protocol").
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Link {
    /// No session: no link yet, its link ended, or the keeper closed its session.
    #[default]
    Down,
    /// Its hello said, not yet welcomed.
    Joining,
    /// Welcomed, its sync said and not yet answered.
    Syncing,
    /// Synced: its reserves and releases are said, and its runs start on their grants.
    Up,
}

/// What the runner's ledger and shell take from an input, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    /// Where the runner stands with its keeper now (`Admission::keeper_said`).
    Keeper(Keeper),
    /// Reservation `id`'s reserve was granted: its start goes ahead now, never sooner (the holder's
    /// item 1).
    Granted(ReservationId),
    /// Reservation `id`'s reserve wasn't granted, and why: its start doesn't happen (item 5).
    NotGranted(ReservationId, NotGranted),
    /// Its sync was answered (CV5): whether the keeper's count and sum of its reservations are its
    /// own, and how many the keeper released (not listed), adopted (listed, not known) and took the
    /// larger amount of. Where they disagree, the keeper's books are the machine's, and its shell
    /// says the difference.
    Synced { agree: bool, released: u64, adopted: u64, disagreed: u64 },
}

/// What an input comes to: the words to say to the keeper on the link, in order, and the events.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Out {
    pub send: Vec<ToKeeper>,
    pub events: Vec<Event>,
}

/// Why a holding didn't start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotStarted {
    /// No sync fits the wire's message bound (`most_held`), in its words.
    Bound(String),
    /// Its records list survivors its core refuses: more than one sync lists, one of an epoch not
    /// before its own, or one listed twice.
    Survivors(hex::Invalid),
}

/// kiln's runner holding room through its machine's keeper (module docs).
#[derive(Debug)]
pub struct Holding {
    core: hex::Holder,
    waits: Waits,
    link: Link,
    /// Reservations said for starts here and not given back: asked, or granted (its run's).
    starts: BTreeSet<ReservationId>,
    /// Reservations whose work can no longer run, their release said or owed once synced, and not
    /// yet answered: still the keeper's.
    releasing: BTreeSet<ReservationId>,
    /// A close that outlives its session: `Superseded` for good, `NotServed` and `Malformed` until
    /// a new link.
    closed: Option<Unsynced>,
    /// The keeper stalled ([`Waits`]), until it answers again.
    stalled: bool,
    /// The most reservations it may hold: the most one sync lists.
    most: u64,
    /// Where it stood when it last said so.
    stood: Option<Keeper>,
    /// The starts that asked nothing, so reserved nothing, in this process: each one's box numbered
    /// ([`BoxName::Unreserved`]).
    unreserved: Numbering,
}

/// The numbering of a runner process's boxes that reserved nothing ([`BoxName::Unreserved`]): its holding's
/// ([`Holding::unreserved`]), shared with whatever else names a box of that process's that no start of its holding's
/// named (a sandbox's launch: kiln plan 01a1cd8e862e7169a074db3d5fa33f3d step 17), so no two boxes share a name.
/// Each number once, from 0; the last one, `u64::MAX`, given again (as a count saturates).
#[derive(Debug, Clone)]
pub struct Numbering {
    epoch: u64,
    next: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl Numbering {
    /// The numbering of the runner process of `epoch`, from 0.
    pub fn new(epoch: u64) -> Self {
        Self { epoch, next: std::sync::Arc::default() }
    }

    /// The epoch its boxes are named under.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The next box's name.
    pub fn next(&self) -> BoxName {
        use std::sync::atomic::Ordering;
        let n = self.next.fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_add(1)).unwrap_or_else(|max| max);
        BoxName::Unreserved { epoch: self.epoch, n }
    }
}

/// What a box kiln runs on its machine is named, so that its machine's keeper pairs the box with
/// what kiln holds there (agreed with Hex's lead): the box of a run that reserved by its
/// reservation, `kiln-r<epoch>-<seq>`; that of a run that asks nothing, which reserves nothing, by
/// its runner process's epoch and its count of such runs in that process, `kiln-z<epoch>-<n>`. Each
/// number is decimal, as a `u64` is written: no sign, no leading zero. At most 47 characters of
/// `a-z 0-9 -`, a name Hex's `BoxId` takes ([`Self::id`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum BoxName {
    /// A run that reserved its room: its reservation.
    Reserved(ReservationId),
    /// The `n`th run (from 0) that asked nothing in the runner process of `epoch`.
    Unreserved { epoch: u64, n: u64 },
}

/// What a box's id pairs with ([`BoxName::pair`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Paired {
    /// A box of kiln's, as its name says.
    Kiln(BoxName),
    /// An id in neither of kiln's namespaces: a box that pairs with nothing of kiln's.
    Unpaired,
}

impl std::fmt::Display for BoxName {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BoxName::Reserved(id) => write!(f, "kiln-r{}-{}", id.epoch, id.seq),
            BoxName::Unreserved { epoch, n } => write!(f, "kiln-z{epoch}-{n}"),
        }
    }
}

impl BoxName {
    /// Its id as Hex's boxes take it.
    pub fn id(self) -> Result<hex_api::BoxId, hex_api::Refusal> {
        hex_api::BoxId::new(&self.to_string())
    }

    /// What box id `id` pairs with: the name it reads as, where that name written gives `id` back
    /// byte for byte (so `kiln-r01-2`, `kiln-r+1-2` and `kiln-r1-2-3` pair with nothing); else
    /// unpaired.
    pub fn pair(id: &str) -> Paired {
        let read = if let Some(rest) = id.strip_prefix("kiln-r") {
            two(rest).map(|(epoch, seq)| BoxName::Reserved(ReservationId { epoch, seq }))
        } else if let Some(rest) = id.strip_prefix("kiln-z") {
            two(rest).map(|(epoch, n)| BoxName::Unreserved { epoch, n })
        } else {
            None
        };
        match read {
            Some(name) if name.to_string() == id => Paired::Kiln(name),
            _ => Paired::Unpaired,
        }
    }
}

/// Two numbers parted by a `-`.
fn two(text: &str) -> Option<(u64, u64)> {
    let (a, b) = text.split_once('-')?;
    Some((a.parse().ok()?, b.parse().ok()?))
}

impl Holding {
    /// The holding of a runner named `name` (the name its certificate carries, which the keeper's
    /// configuration lists) whose process started under `epoch` (durable, one more at every start
    /// of its process: item 2), with the reservations whose work outlived its last process
    /// (`survivors`, from its own records: item 3), each held for no start here and released once
    /// its work ends ([`Self::release`]). Not linked yet, it stands `Joining`.
    pub fn start(name: String, epoch: u64, survivors: &[Held]) -> Result<Self, NotStarted> {
        let most = hex_core::capacity::most_held().map_err(|fault| NotStarted::Bound(fault.to_string()))?;
        let core = hex::Holder::start(name, epoch, most, survivors).map_err(NotStarted::Survivors)?;
        Ok(Self {
            core,
            waits: Waits::default(),
            link: Link::Down,
            starts: BTreeSet::new(),
            releasing: BTreeSet::new(),
            closed: None,
            stalled: false,
            most: u64::try_from(most).unwrap_or(u64::MAX),
            stood: None,
            unreserved: Numbering::new(epoch),
        })
    }

    /// Its boxes that reserve nothing numbered by `numbering` (shared with its process's other namers), where it
    /// numbers its own epoch's; else by its own, as a numbering of another epoch names another process's boxes.
    #[must_use]
    pub fn numbered_by(mut self, numbering: Numbering) -> Self {
        if numbering.epoch == self.core.epoch() {
            self.unreserved = numbering;
        }
        self
    }

    /// Where its core's link stands.
    pub fn link(&self) -> Link {
        self.link
    }

    /// The epoch its process started under.
    pub fn epoch(&self) -> u64 {
        self.core.epoch()
    }

    /// The box of a start that asks nothing ([`Self::reserve`]'s none), which starts without a
    /// reservation: the next of this process's ([`BoxName::Unreserved`]).
    pub fn unreserved(&mut self) -> BoxName {
        self.unreserved.next()
    }

    /// The reservations whose work may run, its starts' granted and its survivors': what its
    /// records list, each written before its run starts (item 3).
    pub fn held(&self) -> impl Iterator<Item = Held> + '_ {
        self.core.held()
    }

    /// Where the runner stands with its keeper (module docs).
    pub fn keeper(&self) -> Keeper {
        if let Some(why) = self.closed {
            return Keeper::Unsynced(why);
        }
        if self.stalled {
            return Keeper::Unsynced(Unsynced::Stalled);
        }
        match (self.link, self.core.view()) {
            (Link::Up, Some(view)) => Keeper::Synced(self.view(view)),
            _ => Keeper::Unsynced(Unsynced::Joining),
        }
    }

    /// The keeper's view as the ledger takes it: the machine's room and everything held there, this
    /// runner's own being its starts' granted reservations. A survivor's, and one whose release
    /// isn't answered yet, count as everyone else's (the view never says more room than the keeper
    /// has), and against the claims it may hold.
    fn view(&self, view: hex::View) -> View {
        let mut own = [0; ask::DIMS];
        let mut survivors = 0u64;
        for held in self.core.held() {
            if self.starts.contains(&held.id) {
                own = ask::plus(&own, &held.amount.to_array());
            } else {
                survivors = survivors.saturating_add(1);
            }
        }
        let releasing = u64::try_from(self.releasing.len()).unwrap_or(u64::MAX);
        View { room: view.room.to_array(), held: view.held.to_array(), own, most_claims: self.most.saturating_sub(survivors).saturating_sub(releasing) }
    }

    /// When its shell wakes it next ([`Self::woke`]): when its oldest word's wait ends. None while
    /// none waits, while stalled (until an answer or the link's end), or before the keeper's first
    /// answer: a cold start waits on liveness alone, its transport ending a link to a keeper gone.
    pub fn due(&self) -> Option<i64> {
        if self.stalled { None } else { self.waits.due() }
    }

    /// A link to the keeper opened at `now` (a connection to its port: CV9): its hello, unless a
    /// newer process of this runner holds its place there (`Superseded`, for good). A link said
    /// open before the last one's end ends that one first, as died.
    pub fn linked(&mut self, now: i64) -> Out {
        let mut out = Out::default();
        let mut events = Vec::new();
        if self.link != Link::Down || self.waits.waiting() > 0 {
            self.lost(WaitEnded::Died, &mut events);
        }
        if self.closed != Some(Unsynced::Superseded) {
            self.closed = None;
            self.hello(now, &mut out);
        }
        self.told(out, events)
    }

    /// Its link ended (`Died`: its transport found the keeper gone, or the connection failed;
    /// `Reset`: the keeper started again, or reset it): each reserve asked is not granted, each
    /// release said is left out of the next sync, what is held stays held (CV5), and nothing new is
    /// admitted until a new link's sync is answered (CV11).
    pub fn ended(&mut self, how: WaitEnded) -> Out {
        let mut events = Vec::new();
        self.lost(how, &mut events);
        self.told(Out::default(), events)
    }

    /// The keeper's word on the link at `now`: the answer to its oldest word waiting, or a hint.
    pub fn heard(&mut self, msg: ToHolder, now: i64) -> Out {
        let mut out = Out::default();
        if matches!(msg, ToHolder::Hint { .. }) {
            // It answers nothing, and shows the keeper going on: the oldest word's window runs from
            // it (Waits).
            self.waits.heard(now);
        } else {
            // The keeper answers in the order said (CV9): this is its oldest word's answer.
            let _ = self.waits.answered(now);
            self.stalled = false;
        }
        let mut core = hex::Out::default();
        self.core.hear(msg, &mut core);
        if self.link == Link::Joining && core.send.iter().any(|word| matches!(word, ToKeeper::Sync { .. })) {
            self.link = Link::Syncing;
        }
        // A close its core took for its own ended its session, its asks unanswered first.
        let closed = core.events.iter().find_map(|event| match event {
            hex::Event::Closed(why) => Some(*why),
            _ => None,
        });
        let unanswered = closed.and_then(outlives).map_or(NotGranted::Unanswered(WaitEnded::Reset), NotGranted::Unsynced);
        let mut events = Vec::new();
        self.events(core.events, unanswered, &mut events);
        self.send(core.send, now, &mut out);
        if let Some(why) = closed {
            match outlives(why) {
                Some(stands) => self.closed = Some(stands),
                None => self.hello(now, &mut out),
            }
        }
        self.told(out, events)
    }

    /// Its shell's wake-up at `now`, at [`Self::due`] or after: where the keeper, its link alive,
    /// has said nothing for a whole window of the slowest answer it has given, it is stalled
    /// ([`Waits`]): each reserve waiting on it is not granted and given back at once (the keeper
    /// may have granted it: CV5), and nothing new is admitted there until it answers again.
    pub fn woke(&mut self, now: i64) -> Out {
        let mut out = Out::default();
        let mut events = Vec::new();
        if !self.stalled && self.waits.stalled(now) {
            self.stalled = true;
            let asked: Vec<ReservationId> = self.starts.iter().copied().filter(|id| !self.core.may_run(*id)).collect();
            let mut core = hex::Out::default();
            for id in asked {
                self.core.ran_out(id, &mut core);
            }
            self.events(core.events, NotGranted::Unanswered(WaitEnded::Stalled), &mut events);
            self.send(core.send, now, &mut out);
        }
        self.told(out, events)
    }

    /// A start's reserve of `amounts` at `now` (the room its live claim holds,
    /// `Admission::live_claim`, or a kernel start's need), said only while synced (CV11). None
    /// where it asks nothing, which reserves nothing (Hex's holder and keeper take no empty amount)
    /// and starts at once, its cold start until kiln has learned what it asks; else its reservation
    /// and the word to say, its start waiting for [`Event::Granted`] (item 1).
    pub fn reserve(&mut self, amounts: Amounts, now: i64) -> Result<Option<(ReservationId, ToKeeper)>, NotGranted> {
        if let Keeper::Unsynced(why) = self.keeper() {
            return Err(NotGranted::Unsynced(why));
        }
        if !keeper::reserves(&amounts) {
            return Ok(None);
        }
        match self.core.reserve(hex::Amounts::from_array(amounts)) {
            Ok((id, word)) => {
                self.starts.insert(id);
                self.waits.said(now);
                Ok(Some((id, word)))
            }
            Err(hex::NotNow::Down) => Err(NotGranted::Unsynced(Unsynced::Joining)),
            Err(hex::NotNow::Full { .. }) => Err(NotGranted::Full),
            Err(hex::NotNow::Empty) => Ok(None),
            Err(hex::NotNow::Exhausted) => Err(NotGranted::Exhausted),
        }
    }

    /// The work of reservation `id` can no longer run (its run ended, its start was granted and not
    /// taken by the ledger, or a survivor's work ended), or its start, still asked, is given up:
    /// its room goes back to the keeper (item 4), at once where synced, else by the next sync. Its
    /// shell writes it out of its records first.
    pub fn release(&mut self, id: ReservationId, now: i64) -> Out {
        let mut out = Out::default();
        self.starts.remove(&id);
        let held = self.core.may_run(id);
        let mut core = hex::Out::default();
        self.core.release(id, &mut core);
        // Its release is said now (synced) or once the sync is answered (syncing), and the keeper
        // holds it until it answers; down or joining, the next sync leaves it out.
        if held && matches!(self.link, Link::Up | Link::Syncing) {
            self.releasing.insert(id);
        }
        let mut events = Vec::new();
        self.events(core.events, NotGranted::Unanswered(WaitEnded::Reset), &mut events);
        self.send(core.send, now, &mut out);
        self.told(out, events)
    }

    fn hello(&mut self, now: i64, out: &mut Out) {
        if let Some(word) = self.core.connect() {
            self.link = Link::Joining;
            self.say(word, now, out);
        }
    }

    fn say(&mut self, word: ToKeeper, now: i64, out: &mut Out) {
        self.waits.said(now);
        out.send.push(word);
    }

    fn send(&mut self, words: Vec<ToKeeper>, now: i64, out: &mut Out) {
        for word in words {
            self.say(word, now, out);
        }
    }

    /// Its link went (`how`): its core's asks unanswered, its releases left out of the next sync,
    /// its words' waits ended, the slowest answer kept (it is the keeper's, not the link's).
    fn lost(&mut self, how: WaitEnded, events: &mut Vec<Event>) {
        let mut core = hex::Out::default();
        self.core.link_down(&mut core);
        self.link = Link::Down;
        self.stalled = false;
        self.releasing.clear();
        let _ = self.waits.ended();
        self.events(core.events, NotGranted::Unanswered(how), events);
    }

    /// Its core's events as the ledger takes them, an ask that ended unanswered `unanswered`.
    fn events(&mut self, core: Vec<hex::Event>, unanswered: NotGranted, events: &mut Vec<Event>) {
        for event in core {
            match event {
                hex::Event::Granted(id) => events.push(Event::Granted(id)),
                hex::Event::Refused(id, why) => {
                    self.starts.remove(&id);
                    events.push(Event::NotGranted(id, refused(why)));
                }
                hex::Event::Unanswered(id) => {
                    self.starts.remove(&id);
                    events.push(Event::NotGranted(id, unanswered));
                }
                hex::Event::Released(id) => {
                    self.releasing.remove(&id);
                }
                hex::Event::Synced { agree, released, adopted, disagreed } => {
                    self.link = Link::Up;
                    events.push(Event::Synced { agree, released, adopted, disagreed });
                }
                // Its session ended, its releases with it (`heard` says what stands after).
                hex::Event::Closed(_) => {
                    self.link = Link::Down;
                    self.releasing.clear();
                }
            }
        }
    }

    /// `out` with where it stands first, where that changed since it last said (so its ledger
    /// takes a refusal's view before the refusal, and waits on the room there is), then `events`.
    fn told(&mut self, mut out: Out, events: Vec<Event>) -> Out {
        let stands = self.keeper();
        if self.stood != Some(stands) {
            self.stood = Some(stands);
            out.events.push(Event::Keeper(stands));
        }
        out.events.extend(events);
        out
    }
}

/// What a keeper's close leaves the runner where it outlives the session (the holder's item 5):
/// none where it says hello again at once (its session gone, or the scope it named refused).
fn outlives(why: Close) -> Option<Unsynced> {
    match why {
        Close::OlderEpoch { .. } => Some(Unsynced::Superseded),
        Close::HoldersFull { .. } | Close::NotAuthorized => Some(Unsynced::NotServed),
        Close::Invalid(_) => Some(Unsynced::Malformed),
        Close::Session | Close::NotSynced | Close::Scope(_) => None,
    }
}

/// A keeper's refusal as the ledger takes it.
fn refused(why: Why) -> NotGranted {
    match why {
        Why::Room => NotGranted::Room,
        Why::Full { .. } => NotGranted::Full,
        Why::Empty => NotGranted::Empty,
        Why::Devices => NotGranted::Devices,
        Why::Disagrees => NotGranted::Disagrees,
    }
}
