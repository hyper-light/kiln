//! The capacity view's simulator (CLAUDE.md §9): the keeper's core over records in memory, beside a host
//! whose measured room and own admissions its caller sets. kiln's simulator drives it as the machine's
//! Hex node; [`super::conformance::view`] holds it to the contract Hex's node keeps.

use std::collections::BTreeMap;

use super::{
    Amounts, Budget, CapacityView, Close, Held, Host, Keeper, Read, Records, Scope, Stop, ToHolder,
    ToKeeper, View,
};
use crate::Refusal;

/// Records in memory, each written whole. What it holds is what the keeper writes, bounded by its budget.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MemoryRecords(BTreeMap<String, String>);

impl Records for MemoryRecords {
    fn put(&mut self, name: &str, value: &str) -> Result<(), Stop> {
        self.0.insert(name.to_owned(), value.to_owned());
        Ok(())
    }

    fn remove(&mut self, name: &str) -> Result<(), Stop> {
        self.0.remove(name);
        Ok(())
    }

    fn all(&mut self) -> Result<Vec<(String, String)>, Stop> {
        Ok(self
            .0
            .iter()
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect())
    }
}

/// A keeper's stop as the seam refuses it.
pub fn refusal(stop: Stop) -> Refusal {
    match stop {
        Stop::Full { what, bound } => Refusal::Exhausted { what, bound },
        Stop::Crashed => Refusal::Broken("the capacity keeper crashed at a write".to_owned()),
        Stop::Damaged(why) | Stop::Io(why) => Refusal::Broken(why),
    }
}

/// A machine's capacity view in memory (module docs).
#[derive(Debug)]
pub struct SimView {
    budget: Budget,
    records: MemoryRecords,
    /// None once it stopped, until [`Host::restart`].
    keeper: Option<Keeper>,
    room: Amounts,
    local: Amounts,
    /// Each link that holds a current session, and that session.
    links: BTreeMap<u64, u64>,
}

impl SimView {
    /// A machine whose room is `room`, its host holding nothing itself.
    pub fn new(budget: Budget, room: Amounts) -> Result<Self, Refusal> {
        let mut records = MemoryRecords::default();
        let mut keeper = Keeper::boot(budget, &mut records).map_err(refusal)?;
        keeper.set_room(room);
        Ok(Self {
            budget,
            records,
            keeper: Some(keeper),
            room,
            local: Amounts::ZERO,
            links: BTreeMap::new(),
        })
    }

    fn keeper(&self) -> Result<&Keeper, Refusal> {
        self.keeper.as_ref().ok_or_else(stopped)
    }

    /// The host's own admissions.
    pub fn local(&self) -> Amounts {
        self.local
    }

    /// The view with the host's own admissions.
    pub fn view(&self) -> Result<View, Refusal> {
        Ok(self.keeper()?.view(self.local))
    }

    /// The budget the host's own admissions are held to: the room less every holder's reservations.
    pub fn room_for_local(&self) -> Result<Amounts, Refusal> {
        Ok(self.keeper()?.room_for_local())
    }

    /// Holder `name`'s reservations, as the keeper holds them.
    pub fn held_by(&self, name: &str) -> Result<Vec<Held>, Refusal> {
        Ok(self.keeper()?.held_by(name).collect())
    }
}

fn stopped() -> Refusal {
    Refusal::Broken("the capacity keeper stopped: restart it".to_owned())
}

impl CapacityView for SimView {
    fn hear(&mut self, link: u64, msg: ToKeeper) -> Result<ToHolder, Refusal> {
        let keeper = self.keeper.as_mut().ok_or_else(stopped)?;
        // A link speaks for its own session alone: a fence of another's is no session of this link's (CV4).
        if let Some(fence) = msg.fence()
            && self.links.get(&link) != Some(&fence.session)
        {
            return Ok(ToHolder::Closed {
                rejected: Some(fence),
                why: Close::Session,
            });
        }
        let answer = match keeper.hear(msg, self.local, &mut self.records) {
            Ok(answer) => answer,
            Err(stop) => {
                self.keeper = None;
                self.links.clear();
                return Err(refusal(stop));
            }
        };
        if let ToHolder::Welcome { fence, .. } = &answer {
            self.links.retain(|_, session| keeper.current(*session));
            self.links.insert(link, fence.session);
        }
        Ok(answer)
    }

    fn link_down(&mut self, link: u64) -> Result<(), Refusal> {
        if let (Some(session), Some(keeper)) = (self.links.remove(&link), self.keeper.as_mut()) {
            keeper.link_down(session);
        }
        Ok(())
    }

    fn hints(&mut self) -> Result<Vec<(u64, ToHolder)>, Refusal> {
        let keeper = self.keeper.as_mut().ok_or_else(stopped)?;
        let mut out = Vec::new();
        for (session, hint) in keeper.hints(self.local) {
            let link = self
                .links
                .iter()
                .find(|(_, s)| **s == session)
                .map(|(link, _)| *link)
                .ok_or_else(|| {
                    Refusal::Broken("a hint owed to a session no link holds".to_owned())
                })?;
            out.push((link, hint));
        }
        Ok(out)
    }
}

impl Host for SimView {
    fn sample(&mut self, room: Amounts, used: &[(u64, Read)]) -> Result<(), Refusal> {
        self.room = room;
        if let Some(keeper) = self.keeper.as_mut() {
            keeper.set_room(room);
            let by_session: Vec<_> = used
                .iter()
                .filter_map(|(link, read)| self.links.get(link).map(|s| (*s, *read)))
                .collect();
            keeper.sampled(&by_session);
        }
        Ok(())
    }

    fn scopes(&mut self) -> Result<Vec<(u64, Scope)>, Refusal> {
        let keeper = self.keeper()?;
        Ok(keeper
            .scopes()
            .into_iter()
            .filter_map(|(session, _, scope)| {
                self.links
                    .iter()
                    .find(|(_, s)| **s == session)
                    .map(|(link, _)| (*link, scope))
            })
            .collect())
    }

    fn set_local(&mut self, local: Amounts) -> Result<(), Refusal> {
        if local != self.local {
            self.local = local;
            if let Some(keeper) = self.keeper.as_mut() {
                keeper.local_changed();
            }
        }
        Ok(())
    }

    fn restart(&mut self) -> Result<(), Refusal> {
        self.links.clear();
        self.keeper = None;
        let mut keeper = Keeper::boot(self.budget, &mut self.records).map_err(refusal)?;
        keeper.set_room(self.room);
        self.keeper = Some(keeper);
        Ok(())
    }
}
