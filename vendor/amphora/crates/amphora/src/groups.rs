//! A store in groups (CP-T1.1; docs/design/store.md, decision 10): stores on one node, each a
//! database file with a writer, a log, a flusher, a checkpointer and readers of its own, keys spread
//! over them by position (their FNV-1a hash), so writes go on in parallel on as many cores. A batch
//! is one group's: a batch with rows in more than one group is refused ([`Refused::Groups`]);
//! operations spanning groups atomically are CP-T1.10's.
//!
//! Which group holds a position is the directory's (CP-T1.9; docs/design/reshard.md): the router
//! routes by the directory it was last given ([`Groups::route`], from the directory group's
//! replicated state, [`crate::reshard::replicated`]), and each group answers only from its own
//! holding ([`Holding`]), checked on every write and read: a router with a stale directory is
//! refused ([`Refused::Away`]), never let write to a group that doesn't hold the key. The holding
//! is kept by the group's own engine with its rows ([`Engine::keep_holding`]), durable before it
//! is acted on, and a group started again answers from what it kept, whatever directory it is
//! started by: never from a router's memory.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::{Duration, Instant};

use crate::meter::Counters;
use crate::reshard::directory::{Change, Directory, Proposal};
use crate::reshard::driver::{Action, Do, Failure, Reply};
use crate::reshard::holding::{Away, Fenced, Holding};
use crate::reshard::{self, GroupId, NodeId, Pos, Span, position};
use crate::store::{Config, Engine, Refused, Store, Ticket};

/// A group: its store, what it holds, and what the copy path's work has cost its writer.
struct Group<E: Engine> {
    store: Store<E>,
    holding: Holding,
    costs: Arc<Mutex<Costs>>,
}

impl<E: Engine> Clone for Group<E> {
    fn clone(&self) -> Self {
        Self {
            store: self.store.clone(),
            holding: self.holding,
            costs: Arc::clone(&self.costs),
        }
    }
}

impl<E: Engine> Group<E> {
    fn new(store: Store<E>, holding: Holding) -> Self {
        Self {
            store,
            holding,
            costs: Arc::default(),
        }
    }
}

/// The copy path's work on a group's writer (CP-T1.9): reading a span's keys, reading the keys a
/// run of the log changed, writing moved keys, sweeping.
#[derive(Debug, Clone, Copy)]
enum Work {
    Export,
    Changed,
    Import,
    Sweep,
}

/// What one kind of work has cost a group's writer, learned from every batch of it run there:
/// its units (positions read or swept, log entries replayed) and how long they held the writer;
/// the units of the last batch; and the most bytes a unit has held in memory in any batch (the
/// keys and rows it moved).
#[derive(Debug, Default, Clone, Copy)]
struct Cost {
    units: u64,
    ns: u64,
    last: u64,
    unit_bytes: u64,
}

impl Cost {
    fn ran(&mut self, units: u64, took: Duration, bytes: u64) {
        self.units = self.units.saturating_add(units);
        self.ns = self
            .ns
            .saturating_add(u64::try_from(took.as_nanos()).unwrap_or(u64::MAX));
        self.last = units;
        let per_unit = bytes.div_ceil(units.max(1));
        self.unit_bytes = self.unit_bytes.max(per_unit);
    }

    /// The units a batch takes to hold the writer about `target` ns, a commit's mean cost there
    /// (docs/design/reshard.md, decision "Batches learned where they run"): the target over the
    /// mean cost of a unit measured so far, at least one. Not yet measured, one unit, and from
    /// then on learned. No commit measured on the node (no write waits on it), the batch doubles
    /// from the last, as TCP's slow start probes (RFC 5681, section 3.1), so a batch's size is
    /// never picked. Either way no more units than `memory` bytes hold at `unit_bytes` a unit, the
    /// most a unit has held in a batch of this work here (decision "A batch bounded by the
    /// process's memory"): at least one unit, whatever a unit holds.
    fn within(&self, target: Option<u128>, memory: u64, unit_bytes: u64) -> u64 {
        let by_time = match target {
            Some(t) if self.units > 0 => {
                let n = t
                    .saturating_mul(u128::from(self.units))
                    .checked_div(u128::from(self.ns.max(1)))
                    .unwrap_or(1);
                u64::try_from(n).unwrap_or(u64::MAX).max(1)
            }
            Some(_) => 1,
            None => self.last.saturating_mul(2).max(1),
        };
        let by_memory = memory.checked_div(unit_bytes.max(1)).unwrap_or(1).max(1);
        by_time.min(by_memory)
    }
}

/// A group's [`Cost`] of each [`Work`].
#[derive(Debug, Default)]
struct Costs([Cost; 4]);

impl Costs {
    /// The most bytes a unit of any work has held here: what a sweep's position is bounded by,
    /// since the keys it sweeps are those this group's copies moved.
    fn unit_bytes(&self) -> u64 {
        self.0.iter().map(|c| c.unit_bytes).max().unwrap_or(0)
    }

    fn of(&mut self, work: Work) -> &mut Cost {
        let [export, changed, import, sweep] = &mut self.0;
        match work {
            Work::Export => export,
            Work::Changed => changed,
            Work::Import => import,
            Work::Sweep => sweep,
        }
    }
}

/// The router's directory and the groups on this node. A write checks and hands over its batch
/// under the read lock, and a change of a holding (a fence) takes the write lock: so every batch
/// let in before a fence is in its group's writer's queue before the fence is.
struct Route<E: Engine> {
    directory: Directory,
    groups: BTreeMap<GroupId, Group<E>>,
    /// Groups retired here that the directory routed by doesn't yet show retired (an id it has
    /// given out, a group it no longer places): a create, release or retire asked of one again
    /// is answered as the driver's contract says, never by making an empty group. Kept in `book`
    /// ([`Book`]) with the directory routed by, before a retire is answered, so a restart keeps
    /// both; an id leaves it once a directory kept with it shows it retired, so it holds no more
    /// than the directory lags this node by.
    retired: BTreeSet<GroupId>,
    book: PathBuf,
}

impl<E: Engine> Route<E> {
    /// Whether `group` was retired, here or as the directory routed by shows it.
    fn was_retired(&self, group: GroupId) -> bool {
        self.retired.contains(&group) || shows_retired(&self.directory, group)
    }
}

/// Whether `directory` shows `group` retired: its id given out and the group placed nowhere.
fn shows_retired(directory: &Directory, group: GroupId) -> bool {
    directory.given_out(group) && directory.node_of(group).is_none()
}

/// What a node keeps of the groups it retired (`Groups::on`'s `retired`): the directory routed by
/// when it was last kept, and the groups retired here that directory doesn't show retired. A
/// restart routes by the newer of it and the directory it is started by, so every group retired
/// here shows retired, by the one or by the other.
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct Book {
    directory: Option<Directory>,
    retired: Vec<u64>,
}

/// The book kept in `book` (an empty one: no book yet, a node started now).
fn read_book(book: &Path) -> anyhow::Result<Book> {
    match std::fs::read(book) {
        Ok(bytes) => postcard::from_bytes(&bytes)
            .map_err(|_| anyhow::anyhow!("a book of retired groups that doesn't decode")),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Book::default()),
        Err(e) => Err(e.into()),
    }
}

/// `directory` and the groups of `retired` it doesn't show retired, kept in `book`, replaced whole
/// and durable before it returns: those it keeps.
fn keep_book(
    book: &Path,
    directory: &Directory,
    retired: &BTreeSet<GroupId>,
) -> anyhow::Result<BTreeSet<GroupId>> {
    let left: BTreeSet<GroupId> = retired
        .iter()
        .copied()
        .filter(|g| !shows_retired(directory, *g))
        .collect();
    let kept = Book {
        directory: Some(directory.clone()),
        retired: left.iter().map(|g| g.0).collect(),
    };
    let bytes = postcard::to_allocvec(&kept)
        .map_err(|_| anyhow::anyhow!("a book of retired groups that doesn't encode"))?;
    reshard::replicated::replace(book, &bytes)?;
    Ok(left)
}

/// The old group's half of a copy or catch-up batch (CP-T1.9; docs/design/reshard.md, "Moves
/// across nodes"): what the new group's node asks of the old group's, answered by
/// [`Groups::serve`] there, at this node's own groups or through [`Peers`] at another's. Its
/// encoding is postcard's, by variant index: variants are only appended.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Pull {
    /// The next batch of a copy: at most `positions` of `rest` (none: the span is copied, only
    /// the log's head is asked). `first`: the operation's first, which holds the old group's log
    /// from the entry it is read at.
    Export {
        from: GroupId,
        rest: Option<Span>,
        first: bool,
        positions: u64,
    },
    /// The keys of `span` that `from`'s log entries after `after` changed, up to `upto`, at most
    /// `entries` of them.
    Changed {
        from: GroupId,
        after: reshard::Seq,
        upto: reshard::Seq,
        span: Span,
        entries: u64,
    },
}

impl Pull {
    /// The old group it asks.
    #[must_use]
    pub fn from(&self) -> GroupId {
        match *self {
            Self::Export { from, .. } | Self::Changed { from, .. } => from,
        }
    }
}

/// A [`Pull`]'s answer.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum Pulled {
    Exported(crate::store::Exported),
    /// None: the old group's log no longer reaches back to `after`.
    Changed(Option<crate::store::Changed>),
    /// The old group isn't on the node asked.
    NoGroup,
}

/// The channel from a node's groups to the other nodes' (CP-T1.9): how a move's new group reads
/// the old one on another node.
pub trait Peers {
    /// `pull`, answered by `node`'s groups ([`Groups::serve`] there) by `until`. An error is the
    /// channel's: the driver asks the action again.
    fn pull(&self, node: NodeId, pull: &Pull, until: Instant) -> anyhow::Result<Pulled>;
}

/// The peers of a node that has none: a pull to another node finds no group there.
#[derive(Debug, Clone, Copy, Default)]
pub struct Alone;

impl Peers for Alone {
    fn pull(&self, _: NodeId, _: &Pull, _: Instant) -> anyhow::Result<Pulled> {
        Ok(Pulled::NoGroup)
    }
}

/// A store in groups: cheap to clone, usable from any thread; clones share one route.
pub struct Groups<E: Engine> {
    route: Arc<RwLock<Route<E>>>,
    config: Config,
    /// The bytes a copy-path batch may hold in memory ([`batch_memory`]).
    memory: u64,
}

impl<E: Engine> Clone for Groups<E> {
    fn clone(&self) -> Self {
        Self {
            route: Arc::clone(&self.route),
            config: self.config,
            memory: self.memory,
        }
    }
}

/// A batch handed to a group's writer: [`Groups::durable`] says what became of it.
#[derive(Debug, Clone)]
pub struct GroupTicket {
    group: GroupId,
    ticket: Ticket,
}

impl GroupTicket {
    /// The group its batch went to.
    #[must_use]
    pub fn group(&self) -> GroupId {
        self.group
    }
}

/// FNV-1a: a key's position stays put from build to build and machine to machine.
pub(crate) fn fnv_bytes(key: &[u8]) -> u64 {
    key.iter().fold(0xcbf2_9ce4_8422_2325u64, |h, b| {
        (h ^ u64::from(*b)).wrapping_mul(0x0000_0100_0000_01b3)
    })
}

/// A directory of `n` groups (at least one) on `node`, the `i`th holding the `i`th of `n` equal
/// runs of positions, made by splits from the one group every database starts as: the same changes
/// the directory group's log carries, so it is a directory like any other.
pub fn even(node: NodeId, n: usize) -> Result<Directory, reshard::Refused> {
    let mut directory = Directory::new(node);
    let n = u128::try_from(n.max(1)).map_err(|_| reshard::Refused::Invalid("too many groups"))?;
    let mut last = GroupId(0);
    for i in 1..n {
        let at = i
            .checked_shl(64)
            .and_then(|i| i.checked_div(n))
            .and_then(|at| Pos::try_from(at).ok())
            .ok_or(reshard::Refused::Invalid("a cut past the last position"))?;
        let begin = directory.propose(&Proposal::Split { group: last, at })?;
        let Change::Begin { op, .. } = &begin else {
            return Err(reshard::Refused::Invalid("a proposal begins an operation"));
        };
        let (id, made) = (op.id, op.to);
        for change in [
            begin,
            Change::Cutover(id),
            Change::Flip(id),
            Change::End(id),
        ] {
            directory.apply(&change)?;
        }
        last = made;
    }
    Ok(directory)
}

/// The bytes a copy-path batch may hold in memory, given the bytes the process may use
/// ([`crate::memory::limit`], measured where it runs: its control groups' limits and its machine's
/// memory; none where the system doesn't say): the budget the process sets aside for what its
/// databases hold in memory at once, its pool of pages (docs/design/store.md, decision 26, with
/// that decision's fallback where the system doesn't say). A batch's keys and rows are such data,
/// read out of one group and written into another (docs/design/reshard.md, decision "A batch
/// bounded by the process's memory").
fn batch_memory(limit: Option<u64>) -> u64 {
    u64::try_from(crate::db::default_pages(limit))
        .unwrap_or(u64::MAX)
        .saturating_mul(crate::db::PAGE_BYTES)
}

/// The bytes `moved` holds in memory: each key and its rows.
fn moved_bytes(moved: &[crate::store::Moved]) -> u64 {
    moved
        .iter()
        .map(|m| {
            let rows = m.rows.as_ref().map_or(0, Vec::len);
            let size = std::mem::size_of::<crate::store::Moved>()
                .saturating_add(m.key.len())
                .saturating_add(rows);
            u64::try_from(size).unwrap_or(u64::MAX)
        })
        .fold(0u64, u64::saturating_add)
}

/// `holding` as an engine keeps it.
fn encode(holding: &Holding) -> anyhow::Result<Vec<u8>> {
    postcard::to_allocvec(holding).map_err(|_| anyhow::anyhow!("a holding that doesn't encode"))
}

/// The holding `engine` keeps, or, if it keeps none (a group made now), one holding `first`, kept
/// before the group takes anything.
fn kept<E: Engine>(engine: &mut E, first: Option<Span>) -> anyhow::Result<Holding> {
    if let Some(bytes) = engine.holding()? {
        return postcard::from_bytes(&bytes)
            .map_err(|_| anyhow::anyhow!("a kept holding that doesn't decode"));
    }
    let holding = Holding::owning(first);
    engine.keep_holding(&encode(&holding)?)?;
    Ok(holding)
}

impl<E: Engine> Groups<E> {
    /// `groups` stores (at least one) over equal runs of positions ([`even`]), the `i`th started
    /// on `engine(i)` with `config`, the groups retired here kept in `retired` ([`Groups::on`]).
    pub fn start(
        groups: usize,
        config: Config,
        retired: &Path,
        mut engine: impl FnMut(usize) -> anyhow::Result<E>,
    ) -> anyhow::Result<Self> {
        let directory = even(NodeId(0), groups)?;
        Self::on(NodeId(0), &directory, config, retired, |g| {
            engine(usize::try_from(g.0).map_err(|_| anyhow::anyhow!("group {g:?}"))?)
        })
    }

    /// A store for each group `directory` places on `node`, started on `engine(group)` with
    /// `config`, each holding what its engine kept, or, if it kept nothing (a group made now), its
    /// span. The groups it places on other nodes are theirs: a key of one is refused here
    /// ([`Away::Elsewhere`]), and a move pulls from one through [`Peers`].
    /// `retired` is the file this node keeps the groups retired here in (one per node; none yet:
    /// a node started now): a group retired before a restart stays retired after it.
    pub fn on(
        node: NodeId,
        directory: &Directory,
        config: Config,
        retired: &Path,
        mut engine: impl FnMut(GroupId) -> anyhow::Result<E>,
    ) -> anyhow::Result<Self> {
        directory.check()?;
        let book = retired.to_path_buf();
        let kept_book = read_book(&book)?;
        let retired: BTreeSet<GroupId> = kept_book.retired.into_iter().map(GroupId).collect();
        // The directory routed by before a restart, if it is newer than the one started by: a
        // router never goes back past what it routed by.
        let directory = match kept_book.directory {
            Some(d) if d.generation() > directory.generation() => {
                d.check()?;
                d
            }
            _ => directory.clone(),
        };
        let directory = &directory;
        let mut groups = BTreeMap::new();
        for (group, on) in directory.groups() {
            if on != node || retired.contains(&group) {
                continue;
            }
            let mut engine = engine(group)?;
            let holding = kept(&mut engine, directory.span_of(group))?;
            let store = Store::start(engine, config)?;
            groups.insert(group, Group::new(store, holding));
        }
        let route = Route {
            directory: directory.clone(),
            groups,
            retired,
            book,
        };
        Ok(Self {
            route: Arc::new(RwLock::new(route)),
            config,
            memory: batch_memory(crate::memory::limit()),
        })
    }

    /// Route by `directory` from now on, if it is newer than the one routed by: the directory
    /// group's, as this member has applied it.
    pub fn route(&self, directory: &Directory) -> Result<(), reshard::Refused> {
        directory.check()?;
        let mut route = self.route.write().unwrap_or_else(PoisonError::into_inner);
        if directory.generation() > route.directory.generation() {
            route.directory = directory.clone();
            // The groups the directory now shows retired leave the book, kept with it. If it
            // can't be kept, the book keeps the older directory with them all, which is as true:
            // they leave at the next directory.
            let shown = route.retired.iter().any(|g| shows_retired(directory, *g));
            if shown && let Ok(left) = keep_book(&route.book, directory, &route.retired) {
                route.retired = left;
            }
        }
        Ok(())
    }

    /// The generation of the directory routed by.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.route
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .directory
            .generation()
    }

    /// A new group (the driver's `Do::Create`), holding nothing until an operation hands it a
    /// span (or what its engine kept, if it is made again after a restart), its store started on
    /// `engine`. Making one that is here already does nothing.
    pub fn create(
        &self,
        group: GroupId,
        engine: impl FnOnce() -> anyhow::Result<E>,
    ) -> anyhow::Result<()> {
        let mut route = self.route.write().unwrap_or_else(PoisonError::into_inner);
        if route.groups.contains_key(&group) {
            return Ok(());
        }
        let mut engine = engine()?;
        let holding = kept(&mut engine, None)?;
        let store = Store::start(engine, self.config)?;
        route.groups.insert(group, Group::new(store, holding));
        Ok(())
    }

    /// `f` on `group`'s holding: how the shell carries out a driver's action at the group (its
    /// receive, copy and catch-up progress, fence, activate, release, unfence or discard). What
    /// `f` changes is kept by the group's engine, after every batch let in before it and durable
    /// with them, before the group acts on it; if it can't be kept (or not by `until`), the group
    /// holds what it held, and the driver asks again (each action is idempotent at its group).
    /// Under the route's write lock, so no batch is let in between. A refusal of `f` is a
    /// [`reshard::Refused`] in the error.
    pub fn hold<T>(
        &self,
        group: GroupId,
        until: Instant,
        f: impl FnOnce(&mut Holding) -> Result<T, reshard::Refused>,
    ) -> anyhow::Result<T> {
        let mut route = self.route.write().unwrap_or_else(PoisonError::into_inner);
        let held = route
            .groups
            .get_mut(&group)
            .ok_or(reshard::Refused::NoGroup(group))?;
        let mut next = held.holding;
        let out = f(&mut next)?;
        if next != held.holding {
            let bytes = encode(&next)?;
            held.store.run(until, move |e| e.keep_holding(&bytes))?;
            held.holding = next;
        }
        Ok(out)
    }

    /// The store of `group`, if it is on this node.
    fn store_of(&self, group: GroupId) -> Option<Store<E>> {
        let route = self.route.read().unwrap_or_else(PoisonError::into_inner);
        route.groups.get(&group).map(|g| g.store.clone())
    }

    /// The holding `group` answers from now, if it is on this node.
    #[must_use]
    pub fn holding_of(&self, group: GroupId) -> Option<Holding> {
        let route = self.route.read().unwrap_or_else(PoisonError::into_inner);
        route.groups.get(&group).map(|g| g.holding)
    }

    /// Carries out a driver's action at this node's groups and answers it (CP-T1.9; the shell of
    /// docs/design/reshard.md, "What is left"): a group's holding through [`Groups::hold`], kept
    /// before it is acted on; a copy or catch-up batch read on the old group's writer, after every
    /// batch handed to it before, and written to the new group's in one transaction; a fence as
    /// an entry of the group's own log, numbered on its writer, so every batch let in before it
    /// is in the log before it. `engine` starts a group's engine (`Do::Create`). A copy or catch-up
    /// is carried at the new group's node and reads the old group through a [`Pull`]: answered
    /// here if the old group is here, else by the node the directory routed by places it on,
    /// through `peers` (a move). A group on no node asked is [`Failure::NoGroup`].
    ///
    /// A batch is as many positions (copy, sweep) or log entries (catch-up) as hold each writer
    /// it runs on about as long as a commit there does, learned from the batches run on it before
    /// ([`Cost::within`]), so a batch delays a write no longer than a commit of its own. An error
    /// is the group's (its engine or its writer): the driver asks again
    /// ([`reshard::driver::Driver::restarted`]).
    pub fn carry(
        &self,
        action: &Action,
        until: Instant,
        peers: &dyn Peers,
        engine: impl FnOnce(GroupId) -> anyhow::Result<E>,
    ) -> anyhow::Result<Reply> {
        let op = action.op;
        let refused = |e: anyhow::Error| match e.downcast::<reshard::Refused>() {
            Ok(reshard::Refused::NoGroup(_)) => Ok(Reply::Failed(Failure::NoGroup)),
            Ok(r) => Ok(Reply::Failed(Failure::Refused(r))),
            Err(e) => Err(e),
        };
        match action.what {
            Do::Create { group, .. } => {
                let retired = self
                    .route
                    .read()
                    .unwrap_or_else(PoisonError::into_inner)
                    .was_retired(group);
                if retired {
                    return Ok(Reply::Failed(Failure::Refused(reshard::Refused::Ended(op))));
                }
                self.create(group, || engine(group))?;
                Ok(Reply::Created)
            }
            Do::Copy { from, to, span } => {
                if self.store_of(to).is_none() {
                    return Ok(Reply::Failed(Failure::NoGroup));
                }
                let progress = match self.hold(to, until, |h| h.receive(op, span)) {
                    Ok(p) => p,
                    Err(e) => return refused(e),
                };
                let rest = match progress.copied {
                    Some(p) if p >= span.hi() => None,
                    Some(p) => p.checked_add(1).and_then(|lo| Span::new(lo, span.hi())),
                    None => Some(span),
                };
                let pull = Pull::Export {
                    from,
                    rest,
                    first: progress.first_at.is_none(),
                    positions: self.batch(to, Work::Import),
                };
                let exported = match self.pull(peers, &pull, until)? {
                    Pulled::Exported(exported) => exported,
                    Pulled::NoGroup => return Ok(Reply::Failed(Failure::NoGroup)),
                    Pulled::Changed(_) => anyhow::bail!("a copy's pull answered as a catch-up's"),
                };
                let units = u64::try_from(exported.keys.len())
                    .unwrap_or(u64::MAX)
                    .max(1);
                let bytes = moved_bytes(&exported.keys);
                let dst = self.store_of(to).ok_or(reshard::Refused::NoGroup(to))?;
                let at = exported.at;
                let (last, done) = (exported.last, exported.done);
                if !exported.keys.is_empty() {
                    let keys = exported.keys;
                    let took = dst.run(until, move |e| {
                        let began = Instant::now();
                        e.import(&keys)?;
                        Ok(began.elapsed())
                    })?;
                    self.ran(to, Work::Import, units, took, bytes);
                }
                match self.hold(to, until, |h| h.copied(op, last, done, at)) {
                    Ok(progress) => Ok(Reply::Copied { progress, head: at }),
                    Err(e) => refused(e),
                }
            }
            Do::Replay {
                from,
                to,
                span,
                upto,
            } => {
                let Some(dst) = self.store_of(to) else {
                    return Ok(Reply::Failed(Failure::NoGroup));
                };
                let Some(incoming) = self
                    .holding_of(to)
                    .and_then(|h| h.incoming())
                    .filter(|i| i.op == op)
                else {
                    return Ok(Reply::Failed(Failure::Refused(reshard::Refused::Ended(op))));
                };
                let after = incoming.progress.replayed;
                let pull = Pull::Changed {
                    from,
                    after,
                    upto,
                    span,
                    entries: self.batch(to, Work::Import),
                };
                let changed = match self.pull(peers, &pull, until)? {
                    Pulled::Changed(Some(changed)) => changed,
                    Pulled::Changed(None) => return Ok(Reply::Failed(Failure::LogGone)),
                    Pulled::NoGroup => return Ok(Reply::Failed(Failure::NoGroup)),
                    Pulled::Exported(_) => anyhow::bail!("a catch-up's pull answered as a copy's"),
                };
                let (through, head) = (changed.through, changed.head);
                let units = through.saturating_sub(after).max(1);
                let bytes = moved_bytes(&changed.keys);
                if !changed.keys.is_empty() {
                    let keys = changed.keys;
                    let took = dst.run(until, move |e| {
                        let began = Instant::now();
                        e.import(&keys)?;
                        Ok(began.elapsed())
                    })?;
                    self.ran(to, Work::Import, units, took, bytes);
                }
                match self.hold(to, until, |h| h.replayed(op, through)) {
                    Ok(progress) => Ok(Reply::Replayed { progress, head }),
                    Err(e) => refused(e),
                }
            }
            Do::Fence { group, span } => match self.fence(group, op, span, until) {
                Ok(at) => Ok(Reply::Fenced { at }),
                Err(e) => refused(e),
            },
            Do::Activate { group } => match self.hold(group, until, |h| h.activate(op)) {
                Ok(()) => Ok(Reply::Activated),
                Err(e) => refused(e),
            },
            Do::Unfence { group } => {
                let Some(store) = self.store_of(group) else {
                    return Ok(Reply::Unfenced);
                };
                if let Err(e) = self.hold(group, until, |h| h.unfence(op)) {
                    return refused(e);
                }
                store.run(until, |e| e.keep_log(None))?;
                Ok(Reply::Unfenced)
            }
            Do::Release { group, span } | Do::Discard { group, span } => {
                let release = matches!(action.what, Do::Release { .. });
                let Some(store) = self.store_of(group) else {
                    // A retired group held nothing when it was retired: a release asked of it
                    // again is done. A group never made has nothing to discard.
                    let retired = {
                        let route = self.route.read().unwrap_or_else(PoisonError::into_inner);
                        route.retired.contains(&group) || route.directory.given_out(group)
                    };
                    return Ok(match (release, retired) {
                        (true, true) => Reply::Released { done: true },
                        (true, false) => Reply::Failed(Failure::NoGroup),
                        (false, _) => Reply::Discarded { done: true },
                    });
                };
                let r = if release {
                    self.hold(group, until, |h| h.release(op))
                } else {
                    self.hold(group, until, |h| h.discard(op))
                };
                if let Err(e) = r {
                    return refused(e);
                }
                let holding = self
                    .holding_of(group)
                    .ok_or(reshard::Refused::NoGroup(group))?;
                let batch = self.batch(group, Work::Sweep);
                let positions = usize::try_from(batch).unwrap_or(usize::MAX);
                let (done, took) = store.run(until, move |e| {
                    let began = Instant::now();
                    if release {
                        e.keep_log(None)?;
                    }
                    let done = e.sweep(span, positions, &|pos| holding.garbage(pos))?;
                    Ok((done, began.elapsed()))
                })?;
                self.ran(group, Work::Sweep, batch, took, 0);
                Ok(if release {
                    Reply::Released { done }
                } else {
                    Reply::Discarded { done }
                })
            }
            Do::Retire { group } => {
                let mut route = self.route.write().unwrap_or_else(PoisonError::into_inner);
                if let Some(held) = route.groups.get(&group) {
                    let h = held.holding;
                    if h.owned().is_some() || h.incoming().is_some() || h.fenced().is_some() {
                        return Ok(Reply::Failed(Failure::Refused(reshard::Refused::Invalid(
                            "a group retired while it holds a span",
                        ))));
                    }
                    route.groups.remove(&group);
                }
                // Kept before it is answered, with the directory routed by: a create asked again
                // after a restart is refused, never made an empty group, whatever directory the
                // node is started by.
                let mut next = route.retired.clone();
                next.insert(group);
                route.retired = keep_book(&route.book, &route.directory, &next)?;
                Ok(Reply::Retired)
            }
        }
    }

    /// Answers `pull` at this node's old group (CP-T1.9): its half of a copy or catch-up batch,
    /// for a new group here or on another node. A copy's batch is read on the group's writer, and
    /// the operation's first holds the group's log from the entry it is read at, on the same turn,
    /// so no entry catch-up needs is let go between them. A batch is the fewer of the units asked
    /// and those that hold this writer about as long as a commit does ([`Cost::within`]), so the
    /// old group's writes wait no longer for it whichever node asks. A group not here is
    /// [`Pulled::NoGroup`].
    pub fn serve(&self, pull: &Pull, until: Instant) -> anyhow::Result<Pulled> {
        self.serve_within(pull, until, u64::MAX)
    }

    /// [`Groups::serve`], its batch also no more units than `bytes` hold at the most a unit of
    /// that work has held here: a channel's bound on one answer (the owner transport's frame,
    /// [`crate::transport::Node`]), so a batch the channel can't carry is never read.
    pub fn serve_within(&self, pull: &Pull, until: Instant, bytes: u64) -> anyhow::Result<Pulled> {
        let from = pull.from();
        let Some(src) = self.store_of(from) else {
            return Ok(Pulled::NoGroup);
        };
        match *pull {
            Pull::Export {
                rest,
                first,
                positions,
                ..
            } => {
                let batch = positions.min(self.batch_within(from, Work::Export, bytes));
                let positions = usize::try_from(batch).unwrap_or(usize::MAX);
                let (exported, took) = src.run(until, move |e| {
                    let began = Instant::now();
                    let head = e.head()?;
                    if first {
                        e.keep_log(Some(head))?;
                    }
                    let exported = match rest {
                        Some(rest) => e.export(rest, positions)?,
                        None => crate::store::Exported {
                            keys: Vec::new(),
                            at: head,
                            last: None,
                            done: true,
                        },
                    };
                    Ok((exported, began.elapsed()))
                })?;
                let units = u64::try_from(exported.keys.len())
                    .unwrap_or(u64::MAX)
                    .max(1);
                self.ran(from, Work::Export, units, took, moved_bytes(&exported.keys));
                Ok(Pulled::Exported(exported))
            }
            Pull::Changed {
                after,
                upto,
                span,
                entries,
                ..
            } => {
                let entries = entries.min(self.batch_within(from, Work::Changed, bytes));
                let (changed, took) = src.run(until, move |e| {
                    let began = Instant::now();
                    let changed = e.changed(after, upto, span, entries)?;
                    Ok((changed, began.elapsed()))
                })?;
                if let Some(c) = &changed {
                    let units = c.through.saturating_sub(after).max(1);
                    self.ran(from, Work::Changed, units, took, moved_bytes(&c.keys));
                }
                Ok(Pulled::Changed(changed))
            }
        }
    }

    /// `pull` answered where its old group is: here, or the node the directory routed by places
    /// it on, through `peers`; [`Pulled::NoGroup`] if it is on no node.
    fn pull(&self, peers: &dyn Peers, pull: &Pull, until: Instant) -> anyhow::Result<Pulled> {
        let from = pull.from();
        if self.store_of(from).is_some() {
            return self.serve(pull, until);
        }
        let node = self
            .route
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .directory
            .node_of(from);
        match node {
            Some(node) => peers.pull(node, pull, until),
            None => Ok(Pulled::NoGroup),
        }
    }

    /// `group` fences `span` for `op`: the fence's entry numbered on the group's writer, after
    /// every batch let in before it (under the route's write lock, so none is let in between),
    /// and the holding kept as that entry. The entry's number.
    fn fence(
        &self,
        group: GroupId,
        op: reshard::OpId,
        span: Span,
        until: Instant,
    ) -> anyhow::Result<reshard::Seq> {
        let mut route = self.route.write().unwrap_or_else(PoisonError::into_inner);
        let held = route
            .groups
            .get_mut(&group)
            .ok_or(reshard::Refused::NoGroup(group))?;
        let holding = held.holding;
        let (at, kept) = held.store.run(until, move |e| {
            let mut next = holding;
            let next_entry = e
                .head()?
                .checked_add(1)
                .ok_or_else(|| anyhow::anyhow!("the log's entries ran out"))?;
            match next.fence(op, span, next_entry)? {
                Fenced::Already(at) => Ok((at, None)),
                Fenced::New(at) => {
                    e.keep_holding(&encode(&next)?)?;
                    Ok((at, Some(next)))
                }
            }
        })?;
        if let Some(kept) = kept {
            held.holding = kept;
        }
        Ok(at)
    }

    /// The units of `work` a batch on `group`'s writer takes ([`Cost::within`]): to hold it
    /// about as long as a commit of its own does, or, if it has committed nothing yet, a commit
    /// of this node's groups (one disk, one machine).
    fn batch(&self, group: GroupId, work: Work) -> u64 {
        self.batch_within(group, work, u64::MAX)
    }

    /// [`Groups::batch`], held to `bytes` as well as to the process's memory.
    fn batch_within(&self, group: GroupId, work: Work, bytes: u64) -> u64 {
        let route = self.route.read().unwrap_or_else(PoisonError::into_inner);
        let mean = |c: &Counters| {
            u128::from(c.apply_us)
                .saturating_mul(1000)
                .checked_div(u128::from(c.commits))
        };
        let Some(held) = route.groups.get(&group) else {
            return 1;
        };
        let target = mean(&held.store.counters()).or_else(|| {
            let (mut us, mut commits) = (0u64, 0u64);
            for g in route.groups.values() {
                let c = g.store.counters();
                us = us.saturating_add(c.apply_us);
                commits = commits.saturating_add(c.commits);
            }
            mean(&Counters {
                commits,
                apply_us: us,
                ..Counters::default()
            })
        });
        let mut costs = held.costs.lock().unwrap_or_else(PoisonError::into_inner);
        // A sweep holds the keys it deletes, which this group's copies moved: a position of it
        // holds no more than the most a moved one held here.
        let unit_bytes = match work {
            Work::Sweep => costs.unit_bytes(),
            _ => costs.of(work).unit_bytes,
        };
        costs
            .of(work)
            .within(target, self.memory.min(bytes), unit_bytes)
    }

    /// `units` of `work`, holding `bytes` in memory, held `group`'s writer for `took`.
    fn ran(&self, group: GroupId, work: Work, units: u64, took: Duration, bytes: u64) {
        let route = self.route.read().unwrap_or_else(PoisonError::into_inner);
        if let Some(held) = route.groups.get(&group) {
            let mut costs = held.costs.lock().unwrap_or_else(PoisonError::into_inner);
            costs.of(work).ran(units, took, bytes);
        }
    }

    /// How many groups are on this node.
    #[must_use]
    pub fn len(&self) -> usize {
        self.route
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .groups
            .len()
    }

    /// Whether no group is on this node (every group's span has moved to other nodes).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The group the directory routed by places `key` in (none only of a directory broken in
    /// memory, which [`Groups::route`] never takes).
    #[must_use]
    pub fn group(&self, key: &str) -> Option<GroupId> {
        let route = self.route.read().unwrap_or_else(PoisonError::into_inner);
        route.directory.owner(position(key))
    }

    /// The store of the group `positions` all route to, once its holding answers every one of
    /// them: under the route's read lock, held while `then` hands the batch over.
    fn routed<T>(
        &self,
        mut positions: impl Iterator<Item = Pos>,
        then: impl FnOnce(GroupId, &Store<E>) -> Result<T, Refused>,
    ) -> Result<T, Refused> {
        let route = self.route.read().unwrap_or_else(PoisonError::into_inner);
        let first = positions.next().unwrap_or(0);
        let group = route.directory.owner(first).ok_or(Refused::Groups)?;
        // A group the directory places on another node doesn't answer here.
        let held = route
            .groups
            .get(&group)
            .ok_or(Refused::Away(Away::Elsewhere))?;
        held.holding.answers(first).map_err(Refused::Away)?;
        for pos in positions {
            if route.directory.owner(pos) != Some(group) {
                return Err(Refused::Groups);
            }
            held.holding.answers(pos).map_err(Refused::Away)?;
        }
        then(group, &held.store)
    }

    fn at<T>(&self, group: GroupId, then: impl FnOnce(&Store<E>) -> T) -> Result<T, Refused> {
        let route = self.route.read().unwrap_or_else(PoisonError::into_inner);
        let held = route.groups.get(&group).ok_or(Refused::Groups)?;
        Ok(then(&held.store))
    }

    /// [`Store::put`] to the rows' group.
    pub fn put(&self, rows: Vec<E::Row>) -> Result<GroupTicket, Refused> {
        let positions: Vec<Pos> = rows.iter().map(|r| position(E::key(r))).collect();
        self.routed(positions.into_iter(), |group, store| {
            let ticket = store.put(rows)?;
            Ok(GroupTicket { group, ticket })
        })
    }

    /// [`Store::put_wait`] to the rows' group. It waits for room under the route's read lock, so a
    /// fence waits for it too, no longer than `until`.
    pub fn put_wait(&self, rows: Vec<E::Row>, until: Instant) -> Result<GroupTicket, Refused> {
        let positions: Vec<Pos> = rows.iter().map(|r| position(E::key(r))).collect();
        self.routed(positions.into_iter(), |group, store| {
            let ticket = store.put_wait(rows, until)?;
            Ok(GroupTicket { group, ticket })
        })
    }

    /// [`Store::put_then`] to the rows' group.
    pub fn put_then(
        &self,
        rows: Vec<E::Row>,
        until: Instant,
        then: impl FnOnce(Result<(), Refused>) + Send + 'static,
    ) -> Result<GroupTicket, Refused> {
        let positions: Vec<Pos> = rows.iter().map(|r| position(E::key(r))).collect();
        self.routed(positions.into_iter(), |group, store| {
            let ticket = store.put_then(rows, until, then)?;
            Ok(GroupTicket { group, ticket })
        })
    }

    /// [`Store::durable`] in the ticket's group.
    pub fn durable(&self, ticket: &GroupTicket, until: Instant) -> Result<(), Refused> {
        let store = self.at(ticket.group, Store::clone)?;
        store.durable(&ticket.ticket, until)
    }

    /// [`Store::sync`] in every group: everything handed over so far, settled and durable.
    pub fn sync(&self, until: Instant) -> Result<(), Refused> {
        self.stores().iter().try_for_each(|s| s.sync(until))
    }

    /// [`Store::read`] in `key`'s group, once its writes handed over before are committed. It
    /// reads under the route's read lock, so no fence and flip can pass it and leave it reading a
    /// span its group no longer holds.
    pub fn read<T: Send + 'static>(
        &self,
        key: &str,
        until: Instant,
        f: impl FnOnce(&mut E::Reader) -> anyhow::Result<T>,
        on_writer: impl FnOnce(&mut E) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        self.routed(std::iter::once(position(key)), |_, store| {
            Ok(store.read(Some(key), until, f, on_writer))
        })?
    }

    /// [`Store::read_committed`] in `key`'s group: what is committed there now.
    pub fn read_committed<T: Send + 'static>(
        &self,
        key: &str,
        until: Instant,
        f: impl FnOnce(&mut E::Reader) -> anyhow::Result<T>,
        on_writer: impl FnOnce(&mut E) -> anyhow::Result<T> + Send + 'static,
    ) -> anyhow::Result<T> {
        self.routed(std::iter::once(position(key)), |_, store| {
            Ok(store.read_committed(until, f, on_writer))
        })?
    }

    /// Every group's store on this node, in group order.
    fn stores(&self) -> Vec<Store<E>> {
        let route = self.route.read().unwrap_or_else(PoisonError::into_inner);
        route.groups.values().map(|g| g.store.clone()).collect()
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
        let stores = self.stores();
        let mut answers = Vec::with_capacity(stores.len());
        for store in &stores {
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
        self.stores().iter().map(Store::counters).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Do: run a node's first sixty-four batches of one work with no commit measured anywhere
    /// (so each doubles from the last), each unit holding 4 KiB, under a budget of 1 MiB. Expect:
    /// the batches grow from one unit by doubling, and never past the 256 units the budget holds,
    /// nor below one, however long they go on; with a budget a unit can't fit in, one unit a
    /// batch (T-T1.9.R8: every resource bounded, docs/design/reshard.md, decision "A batch bounded
    /// by the process's memory").
    #[test]
    fn batches_with_no_commit_double_only_as_far_as_the_memory_holds() {
        let memory = 1 << 20;
        let mut cost = Cost::default();
        let mut sizes = Vec::new();
        for _ in 0..64 {
            let units = cost.within(None, memory, cost.unit_bytes);
            sizes.push(units);
            cost.ran(units, Duration::from_micros(1), units * 4096);
        }
        assert_eq!(sizes.get(..4), Some(&[1, 2, 4, 8][..]));
        assert_eq!(sizes.iter().max(), Some(&256));
        assert_eq!(sizes.last(), Some(&256));
        assert!(sizes.iter().all(|&n| n >= 1));
        assert_eq!(cost.within(None, 100, cost.unit_bytes), 1);
        // With room for every batch, the doubling alone would have gone on past the bound.
        assert!(cost.within(None, u64::MAX, 1) > 256);
    }

    /// Do: learn a batch from the time it took (a commit's mean known) while each unit holds more
    /// bytes than the budget leaves room for. Expect: the memory's bound, not the time's
    /// (T-T1.9.R8).
    #[test]
    fn a_batch_learned_from_time_is_still_bounded_by_memory() {
        let mut cost = Cost::default();
        cost.ran(10, Duration::from_micros(10), 10 * 1000);
        // A commit of 1 ms holds 1,000 units of 1 µs: by time, 1,000.
        assert_eq!(
            cost.within(Some(1_000_000), u64::MAX, cost.unit_bytes),
            1000
        );
        assert_eq!(cost.within(Some(1_000_000), 50_000, cost.unit_bytes), 50);
    }

    /// Do: size the batches' budget from a process limit of 64 GiB, and with no limit said.
    /// Expect: decision 26's pool, a quarter of the limit; its fallback where none is said.
    #[test]
    fn a_batch_is_bounded_by_the_process_pool_of_pages() {
        assert_eq!(batch_memory(Some(64 << 30)), 16 << 30);
        assert_eq!(
            batch_memory(None),
            u64::try_from(crate::db::default_pages(None)).unwrap() * crate::db::PAGE_BYTES
        );
    }
}
