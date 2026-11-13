//! The machine's capacity view in the node agent (joint plan step 13, `docs/design/capacity-view.md`;
//! AC-T7.4.1): the keeper's shell. Its records are a folder of their own, written whole by the node's
//! record files (CV2). The room it keeps is the node's facts' capacity, the machine's physical room, and the
//! node admits its own boxes against that room less every holder's reservations (CV3; the Driver's
//! `admission_spec`). Each admission, a holder's or the node's, checks and commits under one hold of
//! the core lock, the node's re-deriving its room at its commit, so the two admissions are one
//! ledger's. Holders reach it over Hex's transport (CV9, `hex_quic::capacity`).

use std::collections::BTreeMap;
use std::path::Path;

use hex_api::capacity::{
    self, Amounts, Forgotten, Keeper, NotForgotten, Read, Scope, ToHolder, ToKeeper,
};
use hex_sim::model::Resources;
use hex_sim::peer::{Records as _, Stop};

use crate::records::{Files, MAX_BYTES};

/// The bytes a holder's record spends besides its name: its epoch, a u64 in decimal, and one space.
const EPOCH_FIELD: usize = 21;

/// The keeper's records in the node's record files.
struct KeeperFiles(Files);

impl capacity::Records for KeeperFiles {
    fn put(&mut self, name: &str, value: &str) -> Result<(), capacity::Stop> {
        self.0.put(name, value).map_err(to_capacity)
    }

    fn remove(&mut self, name: &str) -> Result<(), capacity::Stop> {
        self.0.remove(name).map_err(to_capacity)
    }

    fn all(&mut self) -> Result<Vec<(String, String)>, capacity::Stop> {
        self.0.all().map_err(to_capacity)
    }
}

fn to_capacity(stop: Stop) -> capacity::Stop {
    match stop {
        Stop::Crashed => capacity::Stop::Crashed,
        Stop::Damaged(why) => capacity::Stop::Damaged(why),
        Stop::Io(why) => capacity::Stop::Io(why),
        Stop::Full { what, bound } => capacity::Stop::Full { what, bound },
    }
}

pub(super) fn to_core(stop: capacity::Stop) -> Stop {
    match stop {
        capacity::Stop::Crashed => Stop::Crashed,
        capacity::Stop::Damaged(why) => Stop::Damaged(why),
        capacity::Stop::Io(why) => Stop::Io(why),
        capacity::Stop::Full { what, bound } => Stop::Full { what, bound },
    }
}

/// A node's resources as the view counts them.
pub(super) fn amounts(r: &Resources) -> Amounts {
    Amounts {
        cpu_milli: r.cpu_milli,
        mem_bytes: r.mem_bytes,
        disk_bytes: r.disk_bytes,
        gpus: u64::from(r.gpus),
    }
}

/// The keeper with its records and the links its holders came on (module docs).
pub(super) struct View {
    keeper: Keeper,
    records: KeeperFiles,
    /// The host's own held as the holders were last owed it.
    told: Amounts,
    /// Each link holding a current session, and that session.
    links: BTreeMap<u64, u64>,
}

impl View {
    /// Opens the keeper's records in `dir`, its bounds derived from the node's (CV8): `holders`, the
    /// holders the node authorizes; each holding at most as many as the node's records keep of its own
    /// boxes (`ran`) and as one sync message lists (`hex_core::capacity::most_held`), so every holder's sync
    /// fits its lane; a name at most a record's bytes less its epoch field.
    pub(super) fn open(dir: &Path, holders: usize, ran: usize) -> Result<Self, Stop> {
        let full = |what| Stop::Full {
            what,
            bound: usize::MAX,
        };
        let name_bytes = MAX_BYTES
            .checked_sub(EPOCH_FIELD)
            .ok_or_else(|| full("a capacity holder's name"))?;
        let synced = hex_core::capacity::most_held()
            .map_err(|e| Stop::Damaged(format!("a capacity sync's bound: {e}")))?;
        let reservations = ran.min(synced);
        let budget = capacity::Budget {
            holders,
            reservations,
            name_bytes,
        };
        // A record per reservation and per holder, and the keeper's epoch.
        let most = holders
            .checked_mul(reservations)
            .and_then(|n| n.checked_add(holders))
            .and_then(|n| n.checked_add(1))
            .ok_or_else(|| full("capacity records"))?;
        let mut records = KeeperFiles(Files::open_with_limit(dir, most)?);
        let keeper = Keeper::boot(budget, &mut records).map_err(to_core)?;
        Ok(Self {
            keeper,
            records,
            told: Amounts::ZERO,
            links: BTreeMap::new(),
        })
    }

    /// The machine's physical room, as the node's facts measured it.
    pub(super) fn set_room(&mut self, room: Amounts) {
        self.sample(room, &[]);
    }

    /// The machine's physical room and what each scoped holder's work used, by its link, read in one sample
    /// (CV12): a synced holder not in it earns no credit.
    pub(super) fn sample(&mut self, room: Amounts, used: &[(u64, Read)]) {
        self.keeper.set_room(room);
        let by_session: Vec<_> = used
            .iter()
            .filter_map(|(link, read)| self.links.get(link).map(|session| (*session, *read)))
            .collect();
        self.keeper.sampled(&by_session);
    }

    /// The links of every synced holder whose hello named a scope, its name, and the scope (CV12).
    pub(super) fn scopes(&self) -> Vec<(u64, String, Scope)> {
        self.keeper
            .scopes()
            .into_iter()
            .filter_map(|(session, name, scope)| {
                self.links
                    .iter()
                    .find(|(_, s)| **s == session)
                    .map(|(link, _)| (*link, name, scope))
            })
            .collect()
    }

    /// The sum of every holder's reservations.
    pub(super) fn foreign(&self) -> Amounts {
        self.keeper.foreign()
    }

    /// What the synced holders' scopes are credited to the room (CV12).
    pub(super) fn credit(&self) -> Amounts {
        self.keeper.credit()
    }

    /// A holder's word on `link`, decided beside the node's own `local`: the answer, and whether the
    /// holders' reservations or their credit changed, so the node's admission must be given its new room first.
    pub(super) fn hear(
        &mut self,
        link: u64,
        msg: ToKeeper,
        local: Amounts,
    ) -> Result<(ToHolder, bool), Stop> {
        // A link speaks for its own session alone: a fence of another's is no session of this link's (CV4).
        if let Some(fence) = msg.fence()
            && self.links.get(&link) != Some(&fence.session)
        {
            let closed = ToHolder::Closed {
                rejected: Some(fence),
                why: capacity::Close::Session,
            };
            return Ok((closed, false));
        }
        let before = (self.keeper.foreign(), self.keeper.credit());
        let answer = self
            .keeper
            .hear(msg, local, &mut self.records)
            .map_err(to_core)?;
        if let ToHolder::Welcome { fence, .. } = &answer {
            let keeper = &self.keeper;
            self.links.retain(|_, session| keeper.current(*session));
            self.links.insert(link, fence.session);
        }
        let after = (self.keeper.foreign(), self.keeper.credit());
        Ok((answer, after != before))
    }

    /// Link `link` dropped: its holder is away and keeps what it holds (CV5), its credit ending at once (CV12).
    /// Whether the credit changed, so the node's admission must be given its new room first.
    pub(super) fn link_down(&mut self, link: u64) -> bool {
        let before = self.keeper.credit();
        if let Some(session) = self.links.remove(&link) {
            self.keeper.link_down(session);
        }
        self.keeper.credit() != before
    }

    /// The hints owed, by link: every synced holder is owed one when the node's own `local` moved since
    /// they were last owed it (a box admitted or ended, a booking risen), as when another holder's moved.
    pub(super) fn hints(&mut self, local: Amounts) -> Result<Vec<(u64, ToHolder)>, Stop> {
        if local != self.told {
            self.told = local;
            self.keeper.local_changed();
        }
        let mut out = Vec::new();
        for (session, hint) in self.keeper.hints(local) {
            let link = self
                .links
                .iter()
                .find(|(_, s)| **s == session)
                .map(|(link, _)| *link)
                .ok_or_else(|| Stop::Damaged("a hint owed to a session no link holds".into()))?;
            out.push((link, hint));
        }
        Ok(out)
    }

    /// The person's release of holder `name`'s reservations (CV6), and whether the holders' reservations
    /// changed.
    pub(super) fn forget(
        &mut self,
        name: &str,
    ) -> Result<(Result<Forgotten, NotForgotten>, bool), Stop> {
        let before = self.keeper.foreign();
        let forgotten = self
            .keeper
            .forget(name, &mut self.records)
            .map_err(to_core)?;
        Ok((forgotten, self.keeper.foreign() != before))
    }

    /// The view the keeper answers with the node's own `local`.
    pub(super) fn view(&self, local: Amounts) -> capacity::View {
        self.keeper.view(local)
    }
}

#[cfg(test)]
#[path = "view_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "race_tests.rs"]
mod race_tests;

#[cfg(test)]
#[path = "credit_tests.rs"]
mod credit_tests;
