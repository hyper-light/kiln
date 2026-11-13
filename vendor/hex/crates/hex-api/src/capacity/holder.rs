//! The holder: the side that reserves through a machine's keeper (`docs/design/capacity-view.md` CV5;
//! kiln's runner, through this core). Its work starts only once the keeper granted its reservation; a
//! reserve it heard no answer to is abandoned, never started, and freed; after every (re)join it lists
//! its whole live set, and checks the keeper's count and sum against its own.
//!
//! Its shell persists its epoch (one more at every start of its process), carries the messages, times
//! each wait for an answer by its learned patience and says when one ran out ([`Holder::ran_out`]), and
//! says when the link dropped ([`Holder::link_down`]).

use std::collections::BTreeMap;

use super::{
    Amounts, Close, Fence, Held, Invalid, ReservationId, Scope, ToHolder, ToKeeper, View, Why,
    count,
};

/// The holder's link (R04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Link {
    Down,
    /// Hello said; no welcome yet.
    Joining,
    /// Welcomed; sync said, no answer yet.
    Syncing(Fence),
    /// Synced: reserves and releases are said.
    Up(Fence),
}

/// A reservation as its holder has it (R04).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    /// Asked; no answer yet. Its work doesn't start.
    Asking,
    /// Granted: its work may run.
    Held,
    /// Its work can no longer run. `sent`: its release is said; else it is owed once synced.
    Releasing { sent: bool },
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    amount: Amounts,
    state: State,
}

/// What the holder's shell acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// Granted: its work may start.
    Granted(ReservationId),
    /// Not granted, and why: its work doesn't start.
    Refused(ReservationId, Why),
    /// No answer came (the link dropped, or its wait ran out): its work doesn't start, and the keeper
    /// frees it.
    Unanswered(ReservationId),
    /// Its release is done.
    Released(ReservationId),
    /// Synced: whether the keeper's count and sum of this holder's reservations are its own, and what
    /// the keeper released, adopted and took the larger amount of.
    Synced {
        agree: bool,
        released: u64,
        adopted: u64,
        disagreed: u64,
    },
    /// The keeper closed the session. `OlderEpoch`: a newer process of this holder is there, and this one
    /// must admit nothing more on this machine.
    Closed(Close),
}

/// What the holder says and what its shell acts on, from one input.
#[derive(Debug, Default)]
pub struct Out {
    pub send: Vec<ToKeeper>,
    pub events: Vec<Event>,
}

/// Why the holder can't reserve now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotNow {
    /// It isn't synced with the machine's keeper: it admits nothing new there (CV5).
    Down,
    /// It holds as many as it may.
    Full { bound: u64 },
    /// The amount asks nothing.
    Empty,
    /// Its epoch has numbered all the ids it can.
    Exhausted,
}

/// The side that reserves through a keeper (module docs).
#[derive(Debug)]
pub struct Holder {
    name: String,
    epoch: u64,
    next: u64,
    bound: usize,
    link: Link,
    entries: BTreeMap<ReservationId, Entry>,
    view: Option<View>,
    stale: u64,
    /// Where its reserved work runs, said in each hello (CV12): given up once the keeper refuses it.
    scope: Option<Scope>,
}

impl Holder {
    /// A holder named `name` whose process started under `epoch` (durable, one more than its last), with
    /// the reservations whose work outlived its last process (`running`, from its own records), holding
    /// at most `bound`.
    pub fn start(
        name: String,
        epoch: u64,
        bound: usize,
        running: &[Held],
    ) -> Result<Self, Invalid> {
        if running.len() > bound {
            return Err(Invalid::SyncTooLong {
                bound: count(bound),
            });
        }
        let mut entries = BTreeMap::new();
        for held in running {
            if held.id.epoch >= epoch {
                return Err(Invalid::FutureId);
            }
            let entry = Entry {
                amount: held.amount,
                state: State::Held,
            };
            if entries.insert(held.id, entry).is_some() {
                return Err(Invalid::DuplicateId);
            }
        }
        Ok(Self {
            name,
            epoch,
            next: 0,
            bound,
            link: Link::Down,
            entries,
            view: None,
            stale: 0,
            scope: None,
        })
    }

    /// Says in each hello that its reserved work runs in `scope`, so its host credits the room what that work uses
    /// (CV12).
    pub fn with_scope(mut self, scope: Scope) -> Self {
        self.scope = Some(scope);
        self
    }

    pub fn epoch(&self) -> u64 {
        self.epoch
    }

    /// Whether it is synced with its keeper.
    pub fn is_up(&self) -> bool {
        matches!(self.link, Link::Up(_))
    }

    /// The machine's view as the keeper last said it.
    pub fn view(&self) -> Option<View> {
        self.view
    }

    /// Messages that referred to no current session or reservation, counted: a session since closed, an
    /// answer said twice.
    pub fn stale(&self) -> u64 {
        self.stale
    }

    /// Whether `id`'s work may run: granted, and not released.
    pub fn may_run(&self, id: ReservationId) -> bool {
        self.entries
            .get(&id)
            .is_some_and(|entry| entry.state == State::Held)
    }

    /// The reservations whose work may run.
    pub fn held(&self) -> impl Iterator<Item = Held> + '_ {
        self.entries
            .iter()
            .filter(|(_, entry)| entry.state == State::Held)
            .map(|(id, entry)| Held {
                id: *id,
                amount: entry.amount,
            })
    }

    /// Opens a session: its hello, where it has none.
    pub fn connect(&mut self) -> Option<ToKeeper> {
        (self.link == Link::Down).then(|| {
            self.link = Link::Joining;
            ToKeeper::Hello {
                holder: self.name.clone(),
                epoch: self.epoch,
                scope: self.scope.clone(),
            }
        })
    }

    /// Asks for `amount`: its id and the reserve to send. Its work starts on [`Event::Granted`] alone.
    pub fn reserve(&mut self, amount: Amounts) -> Result<(ReservationId, ToKeeper), NotNow> {
        let Link::Up(fence) = self.link else {
            return Err(NotNow::Down);
        };
        if amount.is_zero() {
            return Err(NotNow::Empty);
        }
        if self.entries.len() >= self.bound {
            return Err(NotNow::Full {
                bound: count(self.bound),
            });
        }
        let id = ReservationId {
            epoch: self.epoch,
            seq: self.next,
        };
        self.next = self.next.checked_add(1).ok_or(NotNow::Exhausted)?;
        self.entries.insert(
            id,
            Entry {
                amount,
                state: State::Asking,
            },
        );
        Ok((id, ToKeeper::Reserve { fence, id, amount }))
    }

    /// `id`'s work can no longer run (or, asked and not yet granted, won't start): its room goes back.
    pub fn release(&mut self, id: ReservationId, out: &mut Out) {
        let Some(entry) = self.entries.get(&id).copied() else {
            return;
        };
        let releasing = |sent| Entry {
            amount: entry.amount,
            state: State::Releasing { sent },
        };
        match (entry.state, self.link) {
            (State::Releasing { .. }, _) => {}
            (State::Held, Link::Up(fence)) => {
                self.entries.insert(id, releasing(true));
                out.send.push(ToKeeper::Release { fence, id });
            }
            // Listed in the sync said: released once it is answered.
            (State::Held, Link::Syncing(_)) => {
                self.entries.insert(id, releasing(false));
            }
            // Left out of the next sync, which releases it.
            (State::Held, Link::Down | Link::Joining) => {
                self.entries.remove(&id);
            }
            // The keeper may grant it yet: freed at once where the link is up, else by the next sync.
            (State::Asking, _) => self.abandon(id, out),
        }
    }

    /// The wait for `id`'s answer ran out: it is abandoned, never started, and freed (CV5, CV6).
    pub fn ran_out(&mut self, id: ReservationId, out: &mut Out) {
        if self
            .entries
            .get(&id)
            .is_some_and(|entry| entry.state == State::Asking)
        {
            self.abandon(id, out);
            out.events.push(Event::Unanswered(id));
        }
    }

    /// The link dropped: what was asked is abandoned, and what was being released is left out of the
    /// next sync. What is held stays held.
    pub fn link_down(&mut self, out: &mut Out) {
        self.link = Link::Down;
        self.entries.retain(|id, entry| match entry.state {
            State::Held => true,
            State::Asking => {
                out.events.push(Event::Unanswered(*id));
                false
            }
            State::Releasing { .. } => false,
        });
    }

    /// The keeper's word.
    pub fn hear(&mut self, msg: ToHolder, out: &mut Out) {
        match msg {
            ToHolder::Welcome { fence, view } => {
                if self.link != Link::Joining || fence.holder != self.epoch {
                    return self.count_stale();
                }
                self.link = Link::Syncing(fence);
                self.view = Some(view);
                let held = self.held().collect();
                out.send.push(ToKeeper::Sync { fence, held });
            }
            ToHolder::Synced {
                fence,
                view,
                count: theirs,
                sum,
                released,
                adopted,
                disagreed,
            } => {
                if self.link != Link::Syncing(fence) {
                    return self.count_stale();
                }
                self.link = Link::Up(fence);
                self.view = Some(view);
                let (listed, own) = self.listed();
                let agree = listed == theirs && own == Some(sum);
                out.events.push(Event::Synced {
                    agree,
                    released,
                    adopted,
                    disagreed,
                });
                for (id, entry) in &mut self.entries {
                    if entry.state == (State::Releasing { sent: false }) {
                        entry.state = State::Releasing { sent: true };
                        out.send.push(ToKeeper::Release { fence, id: *id });
                    }
                }
            }
            ToHolder::Granted {
                fence,
                id,
                amount,
                view,
            } => {
                if self.link != Link::Up(fence) {
                    return self.count_stale();
                }
                self.view = Some(view);
                match self
                    .entries
                    .get(&id)
                    .map(|entry| (entry.state, entry.amount))
                {
                    Some((State::Asking, asked)) if asked == amount => {
                        self.entries.insert(
                            id,
                            Entry {
                                amount,
                                state: State::Held,
                            },
                        );
                        out.events.push(Event::Granted(id));
                    }
                    // Granted another amount than asked: given back, and its work doesn't start.
                    Some((State::Asking, _)) => {
                        self.entries.remove(&id);
                        out.send.push(ToKeeper::Release { fence, id });
                        out.events.push(Event::Refused(id, Why::Disagrees));
                    }
                    Some(_) => self.count_stale(),
                    // Abandoned before its grant came: given back.
                    None => out.send.push(ToKeeper::Release { fence, id }),
                }
            }
            ToHolder::Refused {
                fence,
                id,
                why,
                view,
            } => {
                if self.link != Link::Up(fence) {
                    return self.count_stale();
                }
                self.view = Some(view);
                match self.entries.get(&id).map(|entry| entry.state) {
                    Some(State::Asking) => {
                        self.entries.remove(&id);
                        out.events.push(Event::Refused(id, why));
                    }
                    // Refused after it was abandoned: nothing was held.
                    None => {}
                    Some(_) => self.count_stale(),
                }
            }
            ToHolder::Released { fence, id, view } => {
                if self.link != Link::Up(fence) {
                    return self.count_stale();
                }
                self.view = Some(view);
                match self.entries.get(&id).map(|entry| entry.state) {
                    Some(State::Releasing { sent: true }) => {
                        self.entries.remove(&id);
                        out.events.push(Event::Released(id));
                    }
                    // The answer to an abandoned reserve's release.
                    None => {}
                    Some(_) => self.count_stale(),
                }
            }
            ToHolder::Hint { fence, view } => {
                if self.link != Link::Up(fence) {
                    return self.count_stale();
                }
                self.view = Some(view);
            }
            ToHolder::Closed { rejected, why } => {
                let ours = match (rejected, self.link) {
                    (None, Link::Joining) => true,
                    (Some(fence), Link::Syncing(current) | Link::Up(current)) => fence == current,
                    _ => false,
                };
                if !ours {
                    return self.count_stale();
                }
                // A refused scope isn't said again: the next hello earns no credit rather than the same close.
                if matches!(why, Close::Scope(_)) {
                    self.scope = None;
                }
                self.link_down(out);
                out.events.push(Event::Closed(why));
            }
        }
    }

    /// Asked, now abandoned: forgotten, and its release said where the link is up (the keeper may have
    /// granted it).
    fn abandon(&mut self, id: ReservationId, out: &mut Out) {
        self.entries.remove(&id);
        if let Link::Up(fence) = self.link {
            out.send.push(ToKeeper::Release { fence, id });
        }
    }

    /// What its sync listed, as it holds it now: the count and the sum of what is held and of what is
    /// released once synced.
    fn listed(&self) -> (u64, Option<Amounts>) {
        let mut n = 0usize;
        let mut sum = Some(Amounts::ZERO);
        for entry in self.entries.values() {
            if matches!(entry.state, State::Held | State::Releasing { sent: false }) {
                n = n.saturating_add(1);
                sum = sum.and_then(|sum| sum.checked_add(entry.amount));
            }
        }
        (count(n), sum)
    }

    fn count_stale(&mut self) {
        self.stale = self.stale.saturating_add(1);
    }
}
