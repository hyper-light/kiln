//! The keeper: a machine's one ledger of the reservations its holders hold (`docs/design/capacity-view.md`
//! CV1-CV8). It admits a holder's reservation only where it fits the room beside everything held, the
//! host's own admissions (`local`, passed into every decision) and every holder's (CV3); writes it before
//! it grants it (CV2); acts only on messages under a holder's current fence (CV4); releases only on its
//! holder's word, never on its silence (CV5); and owes every other synced holder a hint when the view
//! changes (CV7).
//!
//! Its records, each written whole or not at all, are its own (its shell gives it a folder of its own),
//! named `epoch` (its own epoch), `holder-<n>` (holder `n`'s epoch and name) and `res-<n>-<epoch>-<seq>`
//! (a reservation's amounts), `n` a u32: at full width the grammar of the node's own records
//! (`own-u32-u64-u64`), so the node's record files hold them. A name it didn't write is damage. A
//! [`Stop`] means the keeper can't go on: its shell boots it again from its records.

use std::collections::BTreeMap;

use super::{
    Amounts, Close, Fence, Held, Invalid, ReservationId, ToHolder, ToKeeper, View, Why, count,
};

/// The keeper's bounds, each derived by its shell (CV8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// Holders it serves: the holders its host's configuration authorizes on this machine.
    pub holders: usize,
    /// Reservations one holder may hold: what the host's records are sized to keep.
    pub reservations: usize,
    /// Bytes of a holder's name: the host's wire message bound, so a hello always fits one message.
    pub name_bytes: usize,
}

/// Why the keeper stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// Its process crashed at a write (a nemesis's).
    Crashed,
    /// Its records say something no keeper wrote.
    Damaged(String),
    Io(String),
    Full {
        what: &'static str,
        bound: usize,
    },
}

/// What [`Keeper::forget`] released of a holder: its epoch then, and its reservations' count and sum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Forgotten {
    pub epoch: u64,
    pub count: u64,
    pub sum: Amounts,
}

/// Why [`Keeper::forget`] released nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotForgotten {
    /// No holder of that name was ever here.
    Unknown,
    /// The holder has a session: its own sync says what it holds.
    Connected,
}

/// The keeper's durable records, by name: each written whole or not at all.
pub trait Records {
    fn put(&mut self, name: &str, value: &str) -> Result<(), Stop>;
    fn remove(&mut self, name: &str) -> Result<(), Stop>;
    /// Every record, by name.
    fn all(&mut self) -> Result<Vec<(String, String)>, Stop>;
}

/// A holder's link as the keeper has it (R04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Link {
    /// No session: its reservations are kept.
    Away,
    /// A session is open; its sync is owed before anything else.
    Joined { session: u64 },
    /// Synced: reserves and releases are served.
    Synced { session: u64 },
}

#[derive(Debug)]
struct Slot {
    name: String,
    epoch: u64,
    link: Link,
    held: BTreeMap<ReservationId, Amounts>,
    /// The sum of `held`.
    sum: Amounts,
    /// A hint is owed: the view changed since this holder last heard it.
    owed: bool,
}

impl Slot {
    fn new(name: String, epoch: u64) -> Self {
        Self {
            name,
            epoch,
            link: Link::Away,
            held: BTreeMap::new(),
            sum: Amounts::ZERO,
            owed: false,
        }
    }

    /// Its session ended: it is away and owes nothing.
    fn away(&mut self) {
        self.link = Link::Away;
        self.owed = false;
    }
}

/// A machine's one ledger of reservations (module docs).
#[derive(Debug)]
pub struct Keeper {
    budget: Budget,
    epoch: u64,
    /// The last session opened in this epoch.
    sessions: u64,
    room: Amounts,
    slots: Vec<Slot>,
    /// The sum of every holder's reservations.
    foreign: Amounts,
}

const EPOCH: &str = "epoch";

fn holder_record(n: usize) -> String {
    format!("holder-{n}")
}

fn reservation_record(n: usize, id: ReservationId) -> String {
    format!("res-{n}-{}-{}", id.epoch, id.seq)
}

fn amounts_text(a: Amounts) -> String {
    format!(
        "{} {} {} {}",
        a.cpu_milli, a.mem_bytes, a.disk_bytes, a.gpus
    )
}

fn damaged(what: impl std::fmt::Display) -> Stop {
    Stop::Damaged(format!("capacity records: {what}"))
}

fn number<T: std::str::FromStr>(text: &str, what: &str) -> Result<T, Stop> {
    text.parse().map_err(|_| damaged(what))
}

/// A holder's name: 1 to `bytes` bytes of visible ASCII, so it is one word in a record and on a log line.
fn named(name: &str, bytes: usize) -> bool {
    !name.is_empty() && name.len() <= bytes && name.bytes().all(|b| b.is_ascii_graphic())
}

fn parse_amounts(text: &str) -> Result<Amounts, Stop> {
    let mut words = text.split(' ');
    let mut next =
        || -> Result<u64, Stop> { number(words.next().unwrap_or(""), "a reservation's amounts") };
    let amounts = Amounts {
        cpu_milli: next()?,
        mem_bytes: next()?,
        disk_bytes: next()?,
        gpus: next()?,
    };
    if words.next().is_some() {
        return Err(damaged("a reservation's amounts"));
    }
    Ok(amounts)
}

/// A holder's number in a record name: a u32 (module docs).
fn holder_number(text: &str) -> Result<usize, Stop> {
    let n: u32 = number(text, "a holder's number")?;
    usize::try_from(n).map_err(|_| damaged("a holder's number"))
}

fn parse_reservation(key: &str) -> Result<(usize, ReservationId), Stop> {
    let mut words = key.split('-');
    let mut next = || words.next().unwrap_or("");
    let n = holder_number(next())?;
    let epoch = number(next(), "a reservation's epoch")?;
    let seq = number(next(), "a reservation's number")?;
    if words.next().is_some() {
        return Err(damaged("a reservation's name"));
    }
    Ok((n, ReservationId { epoch, seq }))
}

impl Keeper {
    /// Boots from `records`: its epoch one more than the last (written before it serves), its holders
    /// and their reservations as they were, every holder away, and no room until its shell measures it.
    pub fn boot(budget: Budget, records: &mut impl Records) -> Result<Self, Stop> {
        if u32::try_from(budget.holders).is_err() {
            return Err(Stop::Full {
                what: "capacity holders, each named by a u32",
                bound: usize::try_from(u32::MAX).unwrap_or(usize::MAX),
            });
        }
        let most = budget
            .holders
            .checked_mul(budget.reservations)
            .ok_or(Stop::Full {
                what: "capacity reservations",
                bound: usize::MAX,
            })?;
        let mut last: Option<u64> = None;
        let mut holders = BTreeMap::<usize, (u64, String)>::new();
        let mut reservations = Vec::new();
        for (name, value) in records.all()? {
            if name == EPOCH {
                if last
                    .replace(number(&value, "the keeper's epoch")?)
                    .is_some()
                {
                    return Err(damaged("two epochs"));
                }
            } else if let Some(n) = name.strip_prefix("holder-") {
                let n = holder_number(n)?;
                let (epoch, holder) = value
                    .split_once(' ')
                    .ok_or_else(|| damaged("a holder's record"))?;
                if !named(holder, budget.name_bytes) || holders.len() >= budget.holders {
                    return Err(damaged("a holder's record"));
                }
                let epoch = number(epoch, "a holder's epoch")?;
                if holders.insert(n, (epoch, holder.to_owned())).is_some() {
                    return Err(damaged("a holder recorded twice"));
                }
            } else if let Some(key) = name.strip_prefix("res-") {
                if reservations.len() >= most {
                    return Err(Stop::Full {
                        what: "capacity reservations",
                        bound: most,
                    });
                }
                let (n, id) = parse_reservation(key)?;
                reservations.push((n, id, parse_amounts(&value)?));
            } else {
                return Err(damaged("a record no keeper writes"));
            }
        }
        let mut slots = Vec::new();
        for (expected, (n, (epoch, name))) in holders.into_iter().enumerate() {
            if n != expected {
                return Err(damaged("holders not numbered from 0"));
            }
            slots.push(Slot::new(name, epoch));
        }
        let mut foreign = Amounts::ZERO;
        for (n, id, amount) in reservations {
            let slot = slots
                .get_mut(n)
                .ok_or_else(|| damaged("a reservation of no holder"))?;
            if id.epoch > slot.epoch || slot.held.len() >= budget.reservations {
                return Err(damaged("a reservation past its holder's epoch or bound"));
            }
            if slot.held.insert(id, amount).is_some() {
                return Err(damaged("a reservation recorded twice"));
            }
            slot.sum = slot
                .sum
                .checked_add(amount)
                .ok_or_else(|| damaged("a holder's sum"))?;
            foreign = foreign
                .checked_add(amount)
                .ok_or_else(|| damaged("the holders' sum"))?;
        }
        let epoch = match last {
            Some(last) => last
                .checked_add(1)
                .ok_or_else(|| damaged("the keeper's epoch"))?,
            None => 1,
        };
        records.put(EPOCH, &epoch.to_string())?;
        Ok(Self {
            budget,
            epoch,
            sessions: 0,
            room: Amounts::ZERO,
            slots,
            foreign,
        })
    }

    /// This keeper's epoch: it grows at each boot.
    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// The room as its host measured it.
    pub fn room(&self) -> Amounts {
        self.room
    }

    /// The room as its host measured it (CV3). A change is owed to every synced holder.
    pub fn set_room(&mut self, room: Amounts) {
        if room != self.room {
            self.room = room;
            self.owe(None);
        }
    }

    /// The host's own admissions changed: every synced holder is owed the view.
    pub fn local_changed(&mut self) {
        self.owe(None);
    }

    /// The sum of every holder's reservations.
    pub fn foreign(&self) -> Amounts {
        self.foreign
    }

    /// The budget the host's own ledger admits against: the room less every holder's reservations, so its admission
    /// is the same rule as a holder's (CV3).
    pub fn room_for_local(&self) -> Amounts {
        self.room.less(self.foreign)
    }

    /// The view with the host's own admissions `local`: the room every admission is decided in. What is held
    /// saturates: past what a u64 counts, none is free.
    pub fn view(&self, local: Amounts) -> View {
        View {
            room: self.room,
            held: Amounts {
                cpu_milli: local.cpu_milli.saturating_add(self.foreign.cpu_milli),
                mem_bytes: local.mem_bytes.saturating_add(self.foreign.mem_bytes),
                disk_bytes: local.disk_bytes.saturating_add(self.foreign.disk_bytes),
                gpus: local.gpus.saturating_add(self.foreign.gpus),
            },
        }
    }

    /// Whether `session` is some holder's current session: a transport's map from its links to sessions
    /// keeps only those, so it holds at most one entry a holder.
    pub fn current(&self, session: u64) -> bool {
        self.slots.iter().any(|slot| match slot.link {
            Link::Joined { session: s } | Link::Synced { session: s } => s == session,
            Link::Away => false,
        })
    }

    /// Holder `name`'s reservations, as the keeper holds them.
    pub fn held_by<'a>(&'a self, name: &'a str) -> impl Iterator<Item = Held> + 'a {
        self.slots
            .iter()
            .filter(move |slot| slot.name == name)
            .flat_map(|slot| {
                slot.held.iter().map(|(id, amount)| Held {
                    id: *id,
                    amount: *amount,
                })
            })
    }

    /// The answer to `msg`, decided beside the host's own admissions `local`. What it changes is written
    /// before it is answered.
    pub fn hear(
        &mut self,
        msg: ToKeeper,
        local: Amounts,
        records: &mut impl Records,
    ) -> Result<ToHolder, Stop> {
        let answer = match msg {
            // A hello's scope is the wire's since CV12, which is withdrawn: no credit is taken from it.
            ToKeeper::Hello {
                holder,
                epoch,
                scope: _,
            } => self.hello(holder, epoch, local, records),
            ToKeeper::Sync { fence, held } => self.sync(fence, held, local, records),
            ToKeeper::Reserve { fence, id, amount } => {
                self.reserve(fence, id, amount, local, records)
            }
            ToKeeper::Release { fence, id } => self.release(fence, id, local, records),
        }?;
        // A current session closed for what it said ends here as its holder's does; a fence that named
        // no current session closes nothing (its number may be another's in this epoch).
        if let ToHolder::Closed {
            rejected: Some(fence),
            why,
        } = &answer
            && *why != Close::Session
        {
            self.link_down(fence.session);
        }
        Ok(answer)
    }

    /// The session `session` ended (its link dropped): its holder is away, and keeps what it holds (CV5).
    pub fn link_down(&mut self, session: u64) {
        for slot in &mut self.slots {
            if let Link::Joined { session: s } | Link::Synced { session: s } = slot.link
                && s == session
            {
                slot.away();
            }
        }
    }

    /// The person's release of every reservation of holder `name`, which is away and isn't coming back
    /// (CV6: a holder gone for good keeps them otherwise). Refused while the holder has a session: its own
    /// word says what it holds. Its epoch stays, so a process of it from before is still fenced, and a
    /// later sync of its own adopts what still runs. What it released is returned for the shell to log.
    pub fn forget(
        &mut self,
        name: &str,
        records: &mut impl Records,
    ) -> Result<Result<Forgotten, NotForgotten>, Stop> {
        let Some(n) = self.slots.iter().position(|slot| slot.name == name) else {
            return Ok(Err(NotForgotten::Unknown));
        };
        let foreign = self.foreign;
        let slot = self.slot(n)?;
        if slot.link != Link::Away {
            return Ok(Err(NotForgotten::Connected));
        }
        let (sum, epoch) = (slot.sum, slot.epoch);
        let count = count(slot.held.len());
        let ids: Vec<ReservationId> = slot.held.keys().copied().collect();
        let foreign = foreign
            .checked_sub(sum)
            .ok_or_else(|| damaged("the holders' sum"))?;
        for id in ids {
            records.remove(&reservation_record(n, id))?;
            slot.held.remove(&id);
        }
        slot.sum = Amounts::ZERO;
        self.foreign = foreign;
        if count > 0 {
            self.owe(Some(n));
        }
        Ok(Ok(Forgotten { epoch, count, sum }))
    }

    /// The hints owed, by session, each the view as it is now; one a holder however many changes.
    pub fn hints(&mut self, local: Amounts) -> Vec<(u64, ToHolder)> {
        let view = self.view(local);
        let keeper = self.epoch;
        let mut out = Vec::new();
        for slot in &mut self.slots {
            if let Link::Synced { session } = slot.link
                && slot.owed
            {
                slot.owed = false;
                let fence = Fence {
                    keeper,
                    session,
                    holder: slot.epoch,
                };
                out.push((session, ToHolder::Hint { fence, view }));
            }
        }
        out
    }

    fn owe(&mut self, except: Option<usize>) {
        for (n, slot) in self.slots.iter_mut().enumerate() {
            if Some(n) != except && matches!(slot.link, Link::Synced { .. }) {
                slot.owed = true;
            }
        }
    }

    /// Whether `amount` fits the room beside `local` and every holder's reservations (CV3).
    fn fits(&self, amount: Amounts, local: Amounts) -> bool {
        local
            .checked_add(self.foreign)
            .and_then(|held| held.checked_add(amount))
            .is_some_and(|total| total.fits(self.room))
    }

    /// The holder whose current session `fence` names, and whether it is synced.
    fn session(&self, fence: Fence) -> Result<(usize, bool), Close> {
        if fence.keeper != self.epoch {
            return Err(Close::Session);
        }
        self.slots
            .iter()
            .enumerate()
            .find_map(|(n, slot)| match slot.link {
                Link::Joined { session } if session == fence.session => Some((n, false, slot)),
                Link::Synced { session } if session == fence.session => Some((n, true, slot)),
                _ => None,
            })
            .filter(|(_, _, slot)| slot.epoch == fence.holder)
            .map(|(n, synced, _)| (n, synced))
            .ok_or(Close::Session)
    }

    fn slot(&mut self, n: usize) -> Result<&mut Slot, Stop> {
        self.slots
            .get_mut(n)
            .ok_or_else(|| damaged("a holder's slot"))
    }

    fn hello(
        &mut self,
        holder: String,
        epoch: u64,
        local: Amounts,
        records: &mut impl Records,
    ) -> Result<ToHolder, Stop> {
        let closed = |why| ToHolder::Closed {
            rejected: None,
            why,
        };
        if !named(&holder, self.budget.name_bytes) {
            return Ok(closed(Close::Invalid(Invalid::Name)));
        }
        let n = match self.slots.iter().position(|slot| slot.name == holder) {
            Some(n) => n,
            None if self.slots.len() >= self.budget.holders => {
                return Ok(closed(Close::HoldersFull {
                    bound: count(self.budget.holders),
                }));
            }
            None => {
                let n = self.slots.len();
                records.put(&holder_record(n), &format!("{epoch} {holder}"))?;
                self.slots.push(Slot::new(holder, epoch));
                n
            }
        };
        let session = self.sessions.checked_add(1).ok_or(Stop::Full {
            what: "capacity sessions",
            bound: usize::MAX,
        })?;
        let slot = self.slot(n)?;
        if epoch < slot.epoch {
            return Ok(closed(Close::OlderEpoch {
                current: slot.epoch,
            }));
        }
        if epoch > slot.epoch {
            records.put(&holder_record(n), &format!("{epoch} {}", slot.name))?;
            slot.epoch = epoch;
        }
        slot.away();
        slot.link = Link::Joined { session };
        self.sessions = session;
        let fence = Fence {
            keeper: self.epoch,
            session,
            holder: epoch,
        };
        Ok(ToHolder::Welcome {
            fence,
            view: self.view(local),
        })
    }

    fn sync(
        &mut self,
        fence: Fence,
        listed: Vec<Held>,
        local: Amounts,
        records: &mut impl Records,
    ) -> Result<ToHolder, Stop> {
        let closed = |why| ToHolder::Closed {
            rejected: Some(fence),
            why,
        };
        let n = match self.session(fence) {
            Ok((n, _)) => n,
            Err(why) => return Ok(closed(why)),
        };
        let bound = self.budget.reservations;
        if listed.len() > bound {
            return Ok(closed(Close::Invalid(Invalid::SyncTooLong {
                bound: count(bound),
            })));
        }
        let foreign = self.foreign;
        let slot = self.slot(n)?;
        // What the holder holds once synced: what it listed, the larger amount where the keeper's
        // differs (CV5). Checked whole before anything is written.
        let mut after = BTreeMap::new();
        let mut sum = Amounts::ZERO;
        let (mut adopted, mut disagreed) = (0u64, 0u64);
        for Held { id, amount } in listed {
            if id.epoch > slot.epoch {
                return Ok(closed(Close::Invalid(Invalid::FutureId)));
            }
            let kept = match slot.held.get(&id) {
                None => {
                    adopted = adopted.saturating_add(1);
                    amount
                }
                Some(have) if *have != amount => {
                    disagreed = disagreed.saturating_add(1);
                    Amounts::max(*have, amount)
                }
                Some(have) => *have,
            };
            if after.insert(id, kept).is_some() {
                return Ok(closed(Close::Invalid(Invalid::DuplicateId)));
            }
            match sum.checked_add(kept) {
                Some(more) => sum = more,
                None => return Ok(closed(Close::Invalid(Invalid::Overflow))),
            }
        }
        let Some(foreign) = foreign
            .checked_sub(slot.sum)
            .and_then(|others| others.checked_add(sum))
        else {
            return Ok(closed(Close::Invalid(Invalid::Overflow)));
        };
        let gone: Vec<ReservationId> = slot
            .held
            .keys()
            .filter(|id| !after.contains_key(id))
            .copied()
            .collect();
        let released = count(gone.len());
        for id in gone {
            records.remove(&reservation_record(n, id))?;
            slot.held.remove(&id);
        }
        for (id, amount) in after {
            if slot.held.get(&id) != Some(&amount) {
                records.put(&reservation_record(n, id), &amounts_text(amount))?;
                slot.held.insert(id, amount);
            }
        }
        slot.sum = sum;
        slot.link = Link::Synced {
            session: fence.session,
        };
        slot.owed = false;
        let held = count(slot.held.len());
        self.foreign = foreign;
        if released > 0 || adopted > 0 || disagreed > 0 {
            self.owe(Some(n));
        }
        Ok(ToHolder::Synced {
            fence,
            view: self.view(local),
            count: held,
            sum,
            released,
            adopted,
            disagreed,
        })
    }

    fn reserve(
        &mut self,
        fence: Fence,
        id: ReservationId,
        amount: Amounts,
        local: Amounts,
        records: &mut impl Records,
    ) -> Result<ToHolder, Stop> {
        let n = match self.session(fence) {
            Ok((n, true)) => n,
            Ok((_, false)) => return Ok(closed(fence, Close::NotSynced)),
            Err(why) => return Ok(closed(fence, why)),
        };
        let view = self.view(local);
        let fits = self.fits(amount, local);
        let bound = self.budget.reservations;
        let foreign = self.foreign.checked_add(amount);
        let slot = self.slot(n)?;
        let refused = |why| ToHolder::Refused {
            fence,
            id,
            why,
            view,
        };
        if id.epoch != slot.epoch {
            return Ok(closed(fence, Close::Invalid(Invalid::IdEpoch)));
        }
        if let Some(have) = slot.held.get(&id) {
            return Ok(if *have == amount {
                ToHolder::Granted {
                    fence,
                    id,
                    amount,
                    view,
                }
            } else {
                refused(Why::Disagrees)
            });
        }
        if amount.is_zero() {
            return Ok(refused(Why::Empty));
        }
        if amount.gpus > 0 {
            return Ok(refused(Why::Devices));
        }
        if slot.held.len() >= bound {
            return Ok(refused(Why::Full {
                bound: count(bound),
            }));
        }
        let (true, Some(foreign), Some(sum)) = (fits, foreign, slot.sum.checked_add(amount)) else {
            return Ok(refused(Why::Room));
        };
        records.put(&reservation_record(n, id), &amounts_text(amount))?;
        slot.held.insert(id, amount);
        slot.sum = sum;
        self.foreign = foreign;
        self.owe(Some(n));
        Ok(ToHolder::Granted {
            fence,
            id,
            amount,
            view: self.view(local),
        })
    }

    fn release(
        &mut self,
        fence: Fence,
        id: ReservationId,
        local: Amounts,
        records: &mut impl Records,
    ) -> Result<ToHolder, Stop> {
        let n = match self.session(fence) {
            Ok((n, true)) => n,
            Ok((_, false)) => return Ok(closed(fence, Close::NotSynced)),
            Err(why) => return Ok(closed(fence, why)),
        };
        let foreign = self.foreign;
        let slot = self.slot(n)?;
        if let Some(amount) = slot.held.get(&id).copied() {
            let sum = slot
                .sum
                .checked_sub(amount)
                .ok_or_else(|| damaged("a holder's sum"))?;
            let foreign = foreign
                .checked_sub(amount)
                .ok_or_else(|| damaged("the holders' sum"))?;
            records.remove(&reservation_record(n, id))?;
            slot.held.remove(&id);
            slot.sum = sum;
            self.foreign = foreign;
            self.owe(Some(n));
        }
        Ok(ToHolder::Released {
            fence,
            id,
            view: self.view(local),
        })
    }
}

fn closed(fence: Fence, why: Close) -> ToHolder {
    ToHolder::Closed {
        rejected: Some(fence),
        why,
    }
}
