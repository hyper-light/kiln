//! The store as its group's state machine (`docs/observe.md` §5): hyper-raft's core in
//! hyper-durable's shell, the group's log the store's write-ahead log.
//!
//! - **apply** applies an entry's command ([`Store::apply_entry`]);
//! - **persist** cuts the store at the last entry applied: its head into files, and the manifest
//!   records the point with the replicated state there, so the log may be compacted behind it;
//! - **image** names the files of what is durable, linked into a snapshot of their own so that
//!   nothing deletes them while a member fetches them ([`Store::link_snapshot`]);
//! - **install** takes an image whose files the owner staged first ([`Machine::staging`]), as
//!   TiKV steps a snapshot's message once its files are received;
//! - **acts_at_start** is false for every entry: a replica acts on nothing before its group.
//!
//! An entry this build cannot apply ([`crate::store::Newer`]) stops the replica rather than skip
//! it, as every replica of this build would: a newer build applies it, and skipping would leave
//! this replica holding another state. The owner says why ([`Machine::fault`]).

use std::path::{Path, PathBuf};
use std::sync::{Arc, PoisonError, RwLock, RwLockReadGuard, RwLockWriteGuard};

use hyper_durable::{EntryRef, Fatal, StateMachine};
use hyper_raft::proto::{ConfChangeV2, ConfState, EntryType};

use crate::hash::Keyed;
use crate::image::Image;
use crate::manifest::{Membership, Point};
use crate::store::{Answer, Store, StoreError};

/// The subfolder of a store's folder its snapshots are linked into.
pub const SNAPSHOTS: &str = "snap";
/// The subfolder files are staged in for an install.
pub const STAGING: &str = "install";

/// A store as its machine and its readers share it: the machine writes under the lock an entry at
/// a time, queries read under it. std's lock prefers a waiting writer to new readers, so queries
/// never hold the group's applying off for longer than one query takes.
pub type Shared = Arc<RwLock<Store>>;

pub struct Machine {
    store: Shared,
    applied: hyper_durable::Point,
    conf: ConfState,
    snaps: PathBuf,
    staging: PathBuf,
    /// Why the machine last failed, in full: [`Fatal`] says it in a word.
    pub fault: Option<String>,
}

pub fn conf_of(m: &Membership) -> ConfState {
    ConfState { voters: m.voters.clone(), learners: m.learners.clone(), voters_outgoing: m.voters_outgoing.clone(), learners_next: m.learners_next.clone(), auto_leave: m.auto_leave }
}

pub fn membership_of(c: &ConfState) -> Membership {
    Membership { voters: c.voters.clone(), learners: c.learners.clone(), voters_outgoing: c.voters_outgoing.clone(), learners_next: c.learners_next.clone(), auto_leave: c.auto_leave }
}

fn point(p: Point) -> hyper_durable::Point {
    hyper_durable::Point { index: p.index, term: p.term }
}

impl Machine {
    /// The store in `dir`, as durable as it was. A store that holds nothing yet starts in
    /// `founding`, the configuration every founding member states alike; one that holds
    /// something has its own.
    pub fn open(dir: &Path, hash: Keyed, founding: &ConfState) -> Result<Self, StoreError> {
        let mut store = Store::open(dir, hash)?;
        let durable = store.durable();
        if durable.index == 0 && store.membership() == &Membership::default() {
            store.set_membership(membership_of(founding));
        }
        let conf = conf_of(store.membership());
        Ok(Self { applied: point(durable), conf, snaps: dir.join(SNAPSHOTS), staging: dir.join(STAGING), store: Arc::new(RwLock::new(store)), fault: None })
    }

    /// The store, for readers on any thread.
    pub fn shared(&self) -> Shared {
        self.store.clone()
    }

    /// The store, read. A lock a panic poisoned still holds a whole store: nothing in it panics
    /// mid-write (the crate's lints), so the store is taken as it is.
    pub fn read(&self) -> RwLockReadGuard<'_, Store> {
        self.store.read().unwrap_or_else(PoisonError::into_inner)
    }

    fn write(&self) -> RwLockWriteGuard<'_, Store> {
        self.store.write().unwrap_or_else(PoisonError::into_inner)
    }

    /// Where the owner stages an image's files before stepping the snapshot that names them.
    pub fn staging(&self) -> &Path {
        &self.staging
    }

    /// Where the files of the snapshot of `at` are, for a member that fetches them.
    pub fn snapshot_dir(&self, at: Point) -> PathBuf {
        self.snaps.join(crate::store::snapshot_name(at))
    }

    pub fn applied(&self) -> hyper_durable::Point {
        self.applied
    }

    fn failed(&mut self, why: &'static str, detail: impl std::fmt::Display) -> Fatal {
        self.fault = Some(format!("{why}: {detail}"));
        Fatal(why)
    }
}

impl StateMachine for Machine {
    /// Each entry's index and what applying it answered.
    type Answer = (u64, Answer);

    fn apply(&mut self, entry: &EntryRef<'_>, answers: &mut Vec<Self::Answer>) -> Result<(), Fatal> {
        self.applied = hyper_durable::Point { index: entry.index, term: entry.term };
        if entry.kind != EntryType::EntryNormal {
            self.write().note_applied(entry.index);
            return Ok(());
        }
        let applied = self.write().apply_entry(entry.index, entry.data);
        match applied {
            Ok(answer) => {
                answers.push((entry.index, answer));
                Ok(())
            }
            Err(newer) => Err(self.failed("an entry this build cannot apply: kiln must be upgraded", newer)),
        }
    }

    fn apply_change(&mut self, at: hyper_durable::Point, _change: &ConfChangeV2, configuration: &ConfState) -> Result<(), Fatal> {
        self.applied = at;
        self.conf = configuration.clone();
        let mut store = self.write();
        store.set_membership(membership_of(configuration));
        store.note_applied(at.index);
        Ok(())
    }

    fn durable(&self) -> hyper_durable::Point {
        point(self.read().durable())
    }

    fn configuration(&self) -> &ConfState {
        &self.conf
    }

    fn acts_at_start(&self, _entry: &EntryRef<'_>) -> bool {
        false
    }

    /// What is durable, at its point: after a compaction's persist, everything applied.
    fn image(&mut self, into: &mut Vec<u8>) -> Result<(hyper_durable::Point, ConfState), Fatal> {
        let store = self.read();
        let linked = store.link_snapshot(&self.snaps);
        store.image().encode(into);
        let at = (point(store.durable()), conf_of(store.durable_membership()));
        drop(store);
        match linked {
            Ok(_) => Ok(at),
            Err(e) => Err(self.failed("the store's files could not be linked into a snapshot", e)),
        }
    }

    fn image_bytes(&self) -> Option<u64> {
        let mut bytes = Vec::new();
        self.read().image().encode(&mut bytes);
        Some(bytes.len() as u64)
    }

    fn install(&mut self, image: &[u8], at: hyper_durable::Point, configuration: &ConfState) -> Result<(), Fatal> {
        let image = match Image::decode(image) {
            Ok(i) => i,
            Err(e) => return Err(self.failed("a snapshot that is no store's image", e)),
        };
        let at_point = Point { index: at.index, term: at.term };
        let installed = self.write().install(&self.staging, &image, at_point, membership_of(configuration));
        if let Err(e) = installed {
            return Err(self.failed("an image could not be installed", e));
        }
        // This member serves the image it installed until it compacts: its files are linked as
        // its own would be.
        let linked = self.read().link_snapshot(&self.snaps);
        if let Err(e) = linked {
            return Err(self.failed("an installed image's files could not be linked into a snapshot", e));
        }
        self.applied = at;
        self.conf = configuration.clone();
        Ok(())
    }

    fn persist(&mut self) -> Result<(), Fatal> {
        let at = Point { index: self.applied.index, term: self.applied.term };
        let cut = self.write().cut(at);
        match cut {
            Ok(_) => Ok(()),
            Err(e) => Err(self.failed("the store could not be cut", e)),
        }
    }
}
