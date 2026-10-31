//! A store in groups (CP-T1.1; docs/design/store.md, decision 10): stores on one node, each a
//! database file with a writer, a log, a flusher, a checkpointer and readers of its own, keys spread
//! over them by hash, so writes go on in parallel on as many cores. A batch is one group's: a batch
//! with rows in more than one group is refused ([`Refused::Groups`]); operations spanning groups
//! atomically are CP-T1.10's. A database's number of groups is its own for good: another number
//! would look for keys in other groups than their rows.

use std::time::Instant;

use crate::meter::Counters;
use crate::store::{Config, Engine, Refused, Store, Ticket};

/// A store in groups: cheap to clone, usable from any thread.
pub struct Groups<E: Engine> {
    stores: Vec<Store<E>>,
}

impl<E: Engine> Clone for Groups<E> {
    fn clone(&self) -> Self {
        Self {
            stores: self.stores.clone(),
        }
    }
}

/// A batch handed to a group's writer: [`Groups::durable`] says what became of it.
#[derive(Debug, Clone)]
pub struct GroupTicket {
    group: usize,
    ticket: Ticket,
}

impl GroupTicket {
    /// The group its batch went to.
    #[must_use]
    pub fn group(&self) -> usize {
        self.group
    }
}

/// FNV-1a: a key's group stays put from build to build and machine to machine.
fn fnv(key: &str) -> u64 {
    key.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

impl<E: Engine> Groups<E> {
    /// `groups` stores (at least one), the `i`th started on `engine(i)` with `config`.
    pub fn start(
        groups: usize,
        config: Config,
        mut engine: impl FnMut(usize) -> anyhow::Result<E>,
    ) -> anyhow::Result<Self> {
        let n = groups.max(1);
        let mut stores = Vec::with_capacity(n);
        for i in 0..n {
            stores.push(Store::start(engine(i)?, config)?);
        }
        Ok(Self { stores })
    }

    /// How many groups.
    #[must_use]
    pub fn len(&self) -> usize {
        self.stores.len()
    }

    /// Never: there is at least one group.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.stores.is_empty()
    }

    /// The group `key` is in.
    #[must_use]
    pub fn group(&self, key: &str) -> usize {
        let n = u64::try_from(self.stores.len()).unwrap_or(1);
        usize::try_from(fnv(key).checked_rem(n).unwrap_or(0)).unwrap_or(0)
    }

    /// Every group's store, in group order.
    #[must_use]
    pub fn stores(&self) -> &[Store<E>] {
        &self.stores
    }

    fn at(&self, group: usize) -> Result<&Store<E>, Refused> {
        self.stores.get(group).ok_or(Refused::Groups)
    }

    /// The one group every row of `rows` is in (the first for none).
    fn group_of(&self, rows: &[E::Row]) -> Result<usize, Refused> {
        let mut groups = rows.iter().map(|r| self.group(E::key(r)));
        let first = groups.next().unwrap_or(0);
        if groups.any(|g| g != first) {
            return Err(Refused::Groups);
        }
        Ok(first)
    }

    /// [`Store::put`] to the rows' group.
    pub fn put(&self, rows: Vec<E::Row>) -> Result<GroupTicket, Refused> {
        let group = self.group_of(&rows)?;
        let ticket = self.at(group)?.put(rows)?;
        Ok(GroupTicket { group, ticket })
    }

    /// [`Store::put_wait`] to the rows' group.
    pub fn put_wait(&self, rows: Vec<E::Row>, until: Instant) -> Result<GroupTicket, Refused> {
        let group = self.group_of(&rows)?;
        let ticket = self.at(group)?.put_wait(rows, until)?;
        Ok(GroupTicket { group, ticket })
    }

    /// [`Store::put_then`] to the rows' group.
    pub fn put_then(
        &self,
        rows: Vec<E::Row>,
        until: Instant,
        then: impl FnOnce(Result<(), Refused>) + Send + 'static,
    ) -> Result<GroupTicket, Refused> {
        let group = self.group_of(&rows)?;
        let ticket = self.at(group)?.put_then(rows, until, then)?;
        Ok(GroupTicket { group, ticket })
    }

    /// [`Store::durable`] in the ticket's group.
    pub fn durable(&self, ticket: &GroupTicket, until: Instant) -> Result<(), Refused> {
        self.at(ticket.group)?.durable(&ticket.ticket, until)
    }

    /// [`Store::sync`] in every group: everything handed over so far, settled and durable.
    pub fn sync(&self, until: Instant) -> Result<(), Refused> {
        self.stores.iter().try_for_each(|s| s.sync(until))
    }

    /// [`Store::read`] in `key`'s group, once its writes handed over before are committed.
    pub fn read<T: Send + 'static>(
        &self,
        key: &str,
        until: Instant,
        f: impl FnOnce(&mut E::Reader) -> anyhow::Result<T>,
        on_writer: impl FnOnce(&mut E) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        self.at(self.group(key))?
            .read(Some(key), until, f, on_writer)
    }

    /// [`Store::read_committed`] in `key`'s group: what is committed there now.
    pub fn read_committed<T: Send + 'static>(
        &self,
        key: &str,
        until: Instant,
        f: impl FnOnce(&mut E::Reader) -> anyhow::Result<T>,
        on_writer: impl FnOnce(&mut E) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        self.at(self.group(key))?
            .read_committed(until, f, on_writer)
    }

    /// `f` on a reader of every group (or `on_writer` on a group's writer while it has none), once
    /// each group's writes handed over before are committed (`committed`: on what each has
    /// committed now, waiting for none): one answer a group, in group order.
    pub fn read_each<T: Send + 'static>(
        &self,
        until: Instant,
        committed: bool,
        f: impl Fn(&mut E::Reader) -> anyhow::Result<T>,
        on_writer: impl Fn(&mut E) -> anyhow::Result<T> + Clone + Send + 'static,
    ) -> anyhow::Result<Vec<T>> {
        let mut answers = Vec::with_capacity(self.stores.len());
        for store in &self.stores {
            let w = on_writer.clone();
            let answer = if committed {
                store.read_committed(until, &f, move |e| w(e))?
            } else {
                store.read(None, until, &f, move |e| w(e))?
            };
            answers.push(answer);
        }
        Ok(answers)
    }

    /// Each group's counters, in group order.
    #[must_use]
    pub fn counters(&self) -> Vec<Counters> {
        self.stores.iter().map(Store::counters).collect()
    }
}
