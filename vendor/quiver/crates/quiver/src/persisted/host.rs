//! The host: one thread of a node's own, a hyper-rt shard running one task, that drives the
//! node's members and publishers (the catalog's and each topic's), feeds what each group committed
//! to the node's views, and answers the node's commands (`super`). Everything it drives runs to
//! completion on its shard (Focal's doc 32 D8): a wake polls every endpoint, the events it brought
//! are acted on, every member takes its turn, what they committed is fed, and what the turn sent
//! leaves at the next poll.

use std::collections::{BTreeMap, HashMap};
use std::future::{Future, poll_fn};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::Arc;
use std::task::{Poll, Waker};
use std::time::{Duration, Instant};

use focal_bus::wire::Bus;
use focal_memory::MemoryBudget;
use futures::channel::oneshot;
use hyper_rt_transport::{Driver, shard_instant};
use hyper_transport::Event;
use quiver_api::{Offset, Refusal, Topic};

use super::catalog::{Applied, Catalog, Record};
use super::group::{
    Cursor, Drv, Fed, Identities, Plan, Publisher, STREAM, Seat, Settled, drop_event,
};
#[cfg(feature = "sim")]
use super::lock;
use super::{Command, GroupFigures, PersistedConfig, Shared};
use crate::ephemeral::{Log, parse_record};
use crate::node::Clock;

/// Format: the node's run in its data directory, raised at every start (its publishers' sessions
/// are new each run: a session's sequences start again at zero).
const RUN_FILE: &str = "run";
/// Format: the catalog's member's directory, each topic's under `topics/`, the logs' keys under
/// `keys/`, and the port a topic's member listens on, in its directory.
const CATALOG_DIR: &str = "catalog";
const TOPICS_DIR: &str = "topics";
const KEYS_DIR: &str = "keys";
const PORT_FILE: &str = "port";
/// The longest the host sleeps with nothing due: what bounds a missed wake's cost.
const IDLE: Duration = Duration::from_millis(100);

/// Under the simulation (`sim`), what the world gave this start (`super::sim::Start`); nothing in
/// a node that runs for real.
#[cfg(feature = "sim")]
pub(crate) type Sim = Option<super::sim::Start>;
/// Nothing: a node that runs for real.
#[cfg(not(feature = "sim"))]
pub(crate) struct Sim;

/// A node that runs for real: no world gave it its start.
#[cfg(feature = "sim")]
pub(crate) const fn real() -> Sim {
    None
}

/// A node that runs for real.
#[cfg(not(feature = "sim"))]
pub(crate) const fn real() -> Sim {
    Sim
}

/// A command waiting for what it asked to be settled.
enum Waiter {
    Publish {
        reply: oneshot::Sender<Result<Offset, Refusal>>,
    },
    Create {
        name: String,
        reply: oneshot::Sender<Result<(), Refusal>>,
    },
    Delete {
        name: String,
        reply: oneshot::Sender<Result<(), Refusal>>,
    },
    /// The node's own record of the port it serves a part at: nothing waits on it.
    Serves,
    /// The node's record of its start: once applied, the node has caught up with its catalog.
    Start,
    /// A mark the node made looking for a topic it doesn't hold: once applied, it holds every
    /// topic created before it.
    Mark {
        reply: oneshot::Sender<Result<(), Refusal>>,
    },
}

/// A topic's member: bound and waiting for its peers' ports, or open.
enum Slot {
    Bound(Box<Drv>),
    Open(Box<Seat>),
}

/// A topic of the catalog, as this node holds it.
struct Held {
    name: String,
    dir: PathBuf,
    /// Where its member listens, once bound.
    port: Option<u16>,
    slot: Option<Slot>,
    publisher: Publisher,
    cursor: Cursor,
    view: Arc<Log>,
    /// Whether this run recorded the port in the catalog.
    announced: bool,
}

/// Which endpoint an event came from.
#[derive(Clone, Copy)]
enum Source {
    CatalogMember,
    CatalogPublisher,
    Member(u64),
    Publisher(u64),
}

pub(crate) struct Host {
    config: PersistedConfig,
    shared: Arc<Shared>,
    ids: Identities,
    waker: Waker,
    clock: Clock,
    budget: MemoryBudget,
    run: u64,
    catalog: Catalog,
    catalog_seat: Seat,
    catalog_publisher: Publisher,
    catalog_cursor: Cursor,
    topics: BTreeMap<u64, Held>,
    tickets: HashMap<u64, Waiter>,
    next_ticket: u64,
    /// Publishes waiting, sent or not.
    publishes: usize,
    /// A feed stopped at its bound: the next turn is due at once.
    more: bool,
    /// Under the simulation, what the world gave this start.
    #[cfg(feature = "sim")]
    sim: Sim,
}

/// What a fatal fault of the host is said as.
type Fault = String;

/// The count kept at `path`, raised and made durable before it's used (temporary, sync, rename,
/// directory sync), so each start's is greater than every earlier start's.
fn raise_run(path: &Path) -> Result<u64, Fault> {
    use focal_platform::fs::{FileMedium, Medium};
    let mut medium = FileMedium;
    let previous = if medium
        .exists(path)
        .map_err(|_| "the node's run could not be looked for")?
    {
        let bytes = medium
            .read(path, 8)
            .map_err(|_| "the node's run could not be read")?;
        u64::from_le_bytes(
            bytes
                .as_slice()
                .try_into()
                .map_err(|_| "the node's run is damaged")?,
        )
    } else {
        0
    };
    let run = previous.checked_add(1).ok_or("the node's runs are spent")?;
    focal_platform::fs::install(&mut medium, path, &run.to_le_bytes())
        .map_err(|_| "the node's run could not be made durable")?;
    Ok(run)
}

/// The port kept in `dir`, if one was.
fn kept_port(dir: &Path) -> Result<Option<u16>, Fault> {
    use focal_platform::fs::{FileMedium, Medium};
    let medium = FileMedium;
    let path = dir.join(PORT_FILE);
    if !medium
        .exists(&path)
        .map_err(|_| "a part's port could not be looked for")?
    {
        return Ok(None);
    }
    let bytes = medium
        .read(&path, 2)
        .map_err(|_| "a part's port could not be read")?;
    Ok(Some(u16::from_le_bytes(
        bytes
            .as_slice()
            .try_into()
            .map_err(|_| "a part's port is damaged")?,
    )))
}

fn keep_port(dir: &Path, port: u16) -> Result<(), Fault> {
    let mut medium = focal_platform::fs::FileMedium;
    focal_platform::fs::install(&mut medium, &dir.join(PORT_FILE), &port.to_le_bytes())
        .map_err(|_| "a part's port could not be made durable".to_owned())
}

fn make_dir(dir: &Path) -> Result<(), Fault> {
    focal_platform::fs::create_dir_all_durable(dir, focal_platform::fs::create_dir_private)
        .map_err(|_| format!("{} could not be made", dir.display()))
}

/// The session node `id`'s publishers take in run `run`: the node above, the run below, so no
/// two nodes' and no two runs' publishers share one. A part's batches don't say which identity
/// published them, so the node knows its own committed batches by their session alone.
fn producer(id: u64, run: u64) -> Result<u64, Fault> {
    if id >= 1 << 16 || run >= 1 << 48 {
        return Err(format!("node {id}'s run {run} names no session"));
    }
    Ok(id.wrapping_shl(48) | run)
}

/// Node `id`'s member's endpoint, or its publisher's, bound at `address`, its QUIC layer seeded
/// with `seed` where one is given (the simulation's). A member's socket is bound first, as Focal's
/// node binds its own (`serve::node_socket`): its endpoint is keyed by the address it holds, which a
/// port of the system's choosing gives only once bound.
fn bind(
    ids: &Identities,
    id: u64,
    address: SocketAddr,
    member: bool,
    seed: Option<[u8; 32]>,
) -> Result<Drv, Fault> {
    let refused = |e: &dyn std::fmt::Debug| format!("{address} could not be bound: {e:?}");
    if member {
        let (socket, bound) = focal_bus::serve::node_socket(address).map_err(|e| refused(&e))?;
        let endpoint = ids.member_endpoint(id, seed, bound)?;
        return Driver::new(endpoint, socket, focal_bus::limits::IO).map_err(|e| refused(&e));
    }
    let endpoint = ids.client_endpoint(id, seed)?;
    Driver::bind(endpoint, address, focal_bus::limits::IO).map_err(|e| refused(&e))
}

fn local_port(driver: &Drv) -> Result<u16, Fault> {
    driver
        .local_addr()
        .map(|a| a.port())
        .map_err(|e| format!("the bound address: {e:?}"))
}

impl Host {
    /// Opens the node: its run raised, its catalog's member and publisher bound and opened, the
    /// catalog applied from what its part keeps as the turns go.
    pub(crate) fn open(
        config: PersistedConfig,
        shared: Arc<Shared>,
        waker: Waker,
        clock: Clock,
        budget: MemoryBudget,
        sim: Sim,
    ) -> Result<Self, Fault> {
        #[cfg(feature = "sim")]
        let mut sim = sim;
        #[cfg(not(feature = "sim"))]
        let Sim = sim;
        let now = shard_instant();
        make_dir(&config.data)?;
        let run = raise_run(&config.data.join(RUN_FILE))?;
        // Under the simulation the identities are made as Ed25519, whose signatures are one length:
        // an ECDSA certificate verify is a byte longer or shorter from one handshake to the next, and
        // so a run's flights, and a tenth of the seeds ran apart across processes (CP-T2.2 step 6).
        #[cfg(feature = "sim")]
        let kind = if sim.is_some() {
            focal_bus::pki::KeyKind::Ed25519
        } else {
            focal_bus::pki::KeyKind::default()
        };
        #[cfg(not(feature = "sim"))]
        let kind = focal_bus::pki::KeyKind::default();
        let ids = Identities::load(
            &config.identities,
            config.identities_key.as_deref(),
            config.nodes.len(),
            kind,
        )?;
        let own = config
            .address(config.id)
            .ok_or("this node isn't among the cluster's nodes")?;
        let catalog_dir = config.data.join(CATALOG_DIR);
        make_dir(&catalog_dir)?;
        let keys = config.data.join(KEYS_DIR);
        make_dir(&keys)?;
        #[cfg(feature = "sim")]
        let mut seed = || sim.as_mut().map(super::sim::Start::endpoint_seed);
        #[cfg(not(feature = "sim"))]
        let seed = || None;
        let member_driver = bind(&ids, config.id, own, true, seed())?;
        let any = SocketAddr::new(own.ip(), 0);
        let client_driver = bind(&ids, config.id, any, false, seed())?;
        let plan = Plan {
            id: config.id,
            peers: config.peers(),
            data: catalog_dir,
            key_file: keys.join("catalog.key"),
            memory: config.part_memory,
            members: config.ids(),
            clients: ids.nodes(),
            lane_window: ids.lane_window()?,
            publishers: ids.publishers(),
            #[cfg(feature = "sim")]
            machine: sim.as_ref().map(super::sim::Start::machine),
        };
        let catalog_seat = Seat::open(member_driver, plan, now)?;
        let mut catalog_publisher =
            Publisher::new(client_driver, producer(config.id, run)?, config.id, now);
        catalog_publisher.set_nodes(config.nodes.clone(), now);
        let mut start = Vec::new();
        Record::Mark {
            node: config.id,
            run,
        }
        .encode(&mut start);
        let started = (clock)();
        catalog_publisher.queue(start, started, 0, now);
        let mut tickets = HashMap::new();
        tickets.insert(0, Waiter::Start);
        Ok(Self {
            catalog: Catalog::new(config.topics),
            config,
            shared,
            ids,
            waker,
            clock,
            budget,
            run,
            catalog_seat,
            catalog_publisher,
            catalog_cursor: Cursor::default(),
            topics: BTreeMap::new(),
            tickets,
            next_ticket: 1,
            publishes: 0,
            more: false,
            #[cfg(feature = "sim")]
            sim,
        })
    }

    /// Runs until the node is told to stop, or a fault it can't go past.
    pub(crate) async fn run(&mut self) -> Result<(), Fault> {
        loop {
            if self.shared.stopping() {
                return Ok(());
            }
            #[cfg(feature = "sim")]
            if self.sim.as_mut().is_some_and(super::sim::Start::awaited) {
                return Err(self.die());
            }
            let began = shard_instant();
            let due = if self.more {
                Some(began)
            } else {
                self.deadline()
            };
            let sleep_ns = due.map_or(IDLE, |at| at.saturating_duration_since(began).min(IDLE));
            let sleep_ns = u64::try_from(sleep_ns.as_nanos())
                .unwrap_or(u64::MAX)
                .max(1);
            let mut sleep = pin!(hyper_rt::futures::sleep(sleep_ns));
            let mut first = true;
            let found = poll_fn(|context| {
                let found = match self.poll_endpoints(context) {
                    Ok(found) => found,
                    Err(fault) => return Poll::Ready(Err(fault)),
                };
                if !found.is_empty() || sleep.as_mut().poll(context).is_ready() {
                    return Poll::Ready(Ok(found));
                }
                // Polled again with nothing ready: a log's write, a command or a stop woke it.
                if !std::mem::take(&mut first) {
                    return Poll::Ready(Ok(found));
                }
                Poll::Pending
            })
            .await?;
            let now = shard_instant();
            self.turn(found, began, now)?;
        }
    }

    /// One event from each endpoint that has one.
    fn poll_endpoints(
        &mut self,
        context: &mut std::task::Context<'_>,
    ) -> Result<Vec<(Source, Event<Bus>)>, Fault> {
        let mut found = Vec::new();
        let socket = |e| format!("an endpoint's socket failed: {e:?}");
        if let Poll::Ready(event) = self.catalog_seat.driver.poll_event(context) {
            found.push((Source::CatalogMember, event.map_err(socket)?));
        }
        if let Poll::Ready(event) = self.catalog_publisher.driver.poll_event(context) {
            found.push((Source::CatalogPublisher, event.map_err(socket)?));
        }
        for (incarnation, held) in &mut self.topics {
            let member = match &mut held.slot {
                Some(Slot::Bound(driver)) => Some(&mut **driver),
                Some(Slot::Open(seat)) => Some(&mut seat.driver),
                None => None,
            };
            if let Some(driver) = member
                && let Poll::Ready(event) = driver.poll_event(context)
            {
                found.push((Source::Member(*incarnation), event.map_err(socket)?));
            }
            if let Poll::Ready(event) = held.publisher.driver.poll_event(context) {
                found.push((Source::Publisher(*incarnation), event.map_err(socket)?));
            }
        }
        Ok(found)
    }

    fn deadline(&self) -> Option<Instant> {
        let wait = self.config.quorum_wait;
        let mut all = vec![
            self.catalog_seat.deadline(),
            self.catalog_publisher.deadline(wait),
        ];
        for held in self.topics.values() {
            if let Some(Slot::Open(seat)) = &held.slot {
                all.push(seat.deadline());
            }
            all.push(held.publisher.deadline(wait));
        }
        #[cfg(feature = "sim")]
        all.push(self.sim.as_ref().and_then(|s| s.next_cut));
        all.into_iter().flatten().min()
    }

    /// A turn: the events found acted on, every member's turn, what they committed fed; under the
    /// simulation, then what the members asked of their logs answered (`Host::settle_logs`).
    fn turn(
        &mut self,
        found: Vec<(Source, Event<Bus>)>,
        began: Instant,
        now: Instant,
    ) -> Result<(), Fault> {
        self.turn_once(found, began, now)?;
        #[cfg(feature = "sim")]
        self.settle_logs(now)?;
        Ok(())
    }

    fn turn_once(
        &mut self,
        found: Vec<(Source, Event<Bus>)>,
        began: Instant,
        now: Instant,
    ) -> Result<(), Fault> {
        self.catalog_seat.member.waited(began, now);
        for held in self.topics.values_mut() {
            if let Some(Slot::Open(seat)) = &mut held.slot {
                seat.member.waited(began, now);
            }
        }
        for (source, event) in found {
            self.dispatch(source, event, now);
        }
        self.drain(now);
        self.catalog_seat.turn(&self.waker, now)?;
        for held in self.topics.values_mut() {
            if let Some(Slot::Open(seat)) = &mut held.slot {
                seat.turn(&self.waker, now)?;
            }
        }
        self.more = false;
        self.feed_catalog()?;
        self.reconcile(now)?;
        self.feed_topics();
        self.take_commands(now);
        self.pump(now);
        self.figures();
        #[cfg(feature = "sim")]
        self.diagnose();
        Ok(())
    }

    /// Under the simulation, everything the node's members and publishers say of themselves, if
    /// the world asked for it (`sim::World::diagnose`).
    #[cfg(feature = "sim")]
    fn diagnose(&mut self) {
        let mut asked = lock(&self.shared.diagnosis);
        if !asked.as_ref().is_some_and(String::is_empty) {
            return;
        }
        let mut said = format!("catalog: {}\n", self.catalog_seat.diagnosis());
        said.push_str(&format!(
            "  publisher {:?} {:?}\n",
            self.catalog_publisher.known(),
            self.catalog_publisher.state()
        ));
        for (incarnation, held) in &mut self.topics {
            if let Some(Slot::Open(seat)) = &mut held.slot {
                said.push_str(&format!("topic {incarnation}: {}\n", seat.diagnosis()));
            }
            let endpoint = held.publisher.endpoint_state();
            said.push_str(&format!(
                "  publisher {:?} {:?}, {:?}\n  publisher heard {:?}\n  publisher counts {:?}\n  publisher {}\n",
                held.publisher.known(),
                held.publisher.state(),
                held.publisher.stats,
                held.publisher.heard,
                held.publisher.counts,
                endpoint
            ));
        }
        *asked = Some(said);
    }

    fn dispatch(&mut self, source: Source, event: Event<Bus>, now: Instant) {
        match source {
            Source::CatalogMember => self.catalog_seat.on_event(event, now),
            Source::CatalogPublisher => {
                let settled = self.catalog_publisher.on_event(event, now);
                self.settle(settled);
            }
            Source::Member(incarnation) => match self
                .topics
                .get_mut(&incarnation)
                .and_then(|h| h.slot.as_mut())
            {
                Some(Slot::Open(seat)) => seat.on_event(event, now),
                Some(Slot::Bound(driver)) => drop_event(driver, event),
                None => {}
            },
            Source::Publisher(incarnation) => {
                if let Some(held) = self.topics.get_mut(&incarnation) {
                    let settled = held.publisher.on_event(event, now);
                    self.settle(settled);
                }
            }
        }
    }

    /// Every event the endpoints hold already, acted on.
    fn drain(&mut self, now: Instant) {
        while let Some(event) = self.catalog_seat.driver.endpoint().poll_event() {
            self.catalog_seat.on_event(event, now);
        }
        let mut settled = Vec::new();
        while let Some(event) = self.catalog_publisher.driver.endpoint().poll_event() {
            settled.extend(self.catalog_publisher.on_event(event, now));
        }
        for held in self.topics.values_mut() {
            match &mut held.slot {
                Some(Slot::Open(seat)) => {
                    while let Some(event) = seat.driver.endpoint().poll_event() {
                        seat.on_event(event, now);
                    }
                }
                Some(Slot::Bound(driver)) => {
                    while let Some(event) = driver.endpoint().poll_event() {
                        drop_event(driver, event);
                    }
                }
                None => {}
            }
            while let Some(event) = held.publisher.driver.endpoint().poll_event() {
                settled.extend(held.publisher.on_event(event, now));
            }
        }
        self.settle(settled);
    }

    fn settle(&mut self, settled: Vec<Settled>) {
        for Settled::Refused(ticket) in settled {
            self.answer(
                ticket,
                Err(Refusal::Invalid {
                    what: "a publish",
                    why: "the part refused its batch".to_owned(),
                }),
                None,
            );
        }
    }

    /// Answers `ticket`: a publish with its offset or refusal, a create or delete with what its
    /// record did (`applied`), or with the refusal.
    fn answer(&mut self, ticket: u64, outcome: Result<Offset, Refusal>, applied: Option<Applied>) {
        let Some(waiter) = self.tickets.remove(&ticket) else {
            return;
        };
        match waiter {
            Waiter::Publish { reply, .. } => {
                self.publishes = self.publishes.saturating_sub(1);
                let _ = reply.send(outcome);
            }
            Waiter::Create { name, reply } => {
                let out = outcome.and(match applied {
                    Some(Applied::Created | Applied::Same) => Ok(()),
                    Some(Applied::Exists) => Err(Refusal::Exists(name)),
                    Some(Applied::Full) => Err(Refusal::Full {
                        what: "the cluster's persisted topics",
                        limit: u64::try_from(self.config.topics).unwrap_or(u64::MAX),
                    }),
                    _ => Err(Refusal::Unavailable(
                        "the catalog took the create as no record".to_owned(),
                    )),
                });
                let _ = reply.send(out);
            }
            Waiter::Delete { name, reply } => {
                let out = outcome.and(match applied {
                    Some(Applied::Deleted(_)) => Ok(()),
                    _ => Err(Refusal::NoTopic(name)),
                });
                let _ = reply.send(out);
            }
            Waiter::Serves => {}
            Waiter::Start => self.shared.settle(),
            Waiter::Mark { reply } => {
                let _ = reply.send(outcome.map(|_| ()));
            }
        }
    }

    fn ticket(&mut self, waiter: Waiter) -> u64 {
        let ticket = self.next_ticket;
        self.next_ticket = self.next_ticket.wrapping_add(1);
        self.tickets.insert(ticket, waiter);
        ticket
    }

    /// Applies what the catalog's group committed, in order: each record's effect on this node,
    /// and the answer to this node's own.
    fn feed_catalog(&mut self) -> Result<(), Fault> {
        let mut records: Vec<(u64, Option<Record>, u64, u32)> = Vec::new();
        let mut skipped = None;
        self.more |= self
            .catalog_cursor
            .feed(self.catalog_seat.part(), |fed| match fed {
                Fed::Skip(floor) => skipped = Some(floor),
                Fed::Batch(batch) => {
                    for (at, record) in batch.records().enumerate() {
                        let offset = batch
                            .base_offset()
                            .saturating_add(u64::try_from(at).unwrap_or(u64::MAX));
                        let record = record.ok().and_then(|r| Record::decode(r.payload));
                        records.push((offset, record, batch.producer(), batch.sequence()));
                    }
                }
            });
        if let Some(floor) = skipped
            && floor > 0
        {
            // The catalog's part let go of records this node never applied: what they decided is
            // lost to it (an open bound: STATUS.md).
            return Err(format!(
                "the catalog's records below {floor} were let go before this node applied them"
            ));
        }
        for (offset, record, by, sequence) in records {
            let applied = self.catalog.apply(offset, record.as_ref());
            match (&record, applied) {
                (Some(Record::Create { name, topic }), Applied::Created) => {
                    self.hold(name, *topic, offset)?
                }
                (_, Applied::Deleted(incarnation)) => self.let_go(incarnation),
                _ => {}
            }
            if by == self.catalog_publisher.producer
                && let Some(ticket) = self.catalog_publisher.committed(sequence)
            {
                self.answer(ticket, Ok(offset), Some(applied));
            }
        }
        Ok(())
    }

    /// Holds the topic the catalog created at `incarnation`: its view, empty, and its publisher,
    /// which sends once every node's port is known.
    fn hold(&mut self, name: &str, topic: Topic, incarnation: u64) -> Result<(), Fault> {
        let dir = self
            .config
            .data
            .join(TOPICS_DIR)
            .join(incarnation.to_string());
        let own = self
            .config
            .address(self.config.id)
            .ok_or("this node's address")?;
        let seed = self.endpoint_seed();
        let driver = bind(
            &self.ids,
            self.config.id,
            SocketAddr::new(own.ip(), 0),
            false,
            seed,
        )?;
        let publisher = Publisher::new(
            driver,
            producer(self.config.id, self.run)?,
            self.config.id,
            shard_instant(),
        );
        let view = Arc::new(Log::new(topic, STREAM, self.budget.clone()));
        if let Some(old) = self.shared.put_view(name, Arc::clone(&view)) {
            old.close();
        }
        self.topics.insert(
            incarnation,
            Held {
                name: name.to_owned(),
                dir,
                port: None,
                slot: None,
                publisher,
                cursor: Cursor::default(),
                view,
                announced: false,
            },
        );
        Ok(())
    }

    /// Lets go of the topic of `incarnation`, deleted: its view ends, its waiting publishes are
    /// refused, its member closes and its directory goes.
    fn let_go(&mut self, incarnation: u64) {
        let Some(held) = self.topics.remove(&incarnation) else {
            return;
        };
        self.shared.drop_view(&held.name, &held.view);
        held.view.close();
        for ticket in held.publisher.tickets() {
            self.answer(ticket, Err(Refusal::NoTopic(held.name.clone())), None);
        }
        let dir = held.dir.clone();
        drop(held);
        // What's left is nobody's: a node that stops before this finishes makes nothing of it, for
        // the catalog no longer names it.
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_file(
            self.config
                .data
                .join(KEYS_DIR)
                .join(format!("{incarnation}.key")),
        );
    }

    /// Brings each held topic's member along: bound, its port recorded in the catalog, opened once
    /// every node's port is known.
    fn reconcile(&mut self, now: Instant) -> Result<(), Fault> {
        let own = self
            .config
            .address(self.config.id)
            .ok_or("this node's address")?;
        let mut announce = Vec::new();
        #[cfg(feature = "sim")]
        let machine = self.sim.as_ref().map(super::sim::Start::machine);
        #[cfg(feature = "sim")]
        let mut seed = || self.sim.as_mut().map(super::sim::Start::endpoint_seed);
        #[cfg(not(feature = "sim"))]
        let seed = || None;
        for (incarnation, held) in &mut self.topics {
            if held.slot.is_none() {
                make_dir(&held.dir)?;
                let kept = kept_port(&held.dir)?;
                let at = |port| SocketAddr::new(own.ip(), port);
                // The port it kept, unless another socket took it while the node was down: then
                // one the system picks, recorded in the catalog as the part's port at this node.
                let bound =
                    kept.map(|port| bind(&self.ids, self.config.id, at(port), true, seed()));
                let driver = match bound {
                    Some(Ok(driver)) => driver,
                    _ => bind(&self.ids, self.config.id, at(0), true, seed())?,
                };
                let port = local_port(&driver)?;
                if kept != Some(port) {
                    keep_port(&held.dir, port)?;
                }
                held.port = Some(port);
                held.slot = Some(Slot::Bound(Box::new(driver)));
            }
            let ports = self.catalog.ports(*incarnation);
            if !held.announced {
                held.announced = true;
                if ports.and_then(|p| p.get(&self.config.id)).copied() != held.port
                    && let Some(port) = held.port
                {
                    announce.push(Record::Serves {
                        incarnation: *incarnation,
                        node: self.config.id,
                        port,
                    });
                }
            }
            let Some(ports) = ports else { continue };
            let everyone: Option<Vec<(u64, SocketAddr)>> = self
                .config
                .nodes
                .iter()
                .map(|(id, address)| {
                    ports
                        .get(id)
                        .map(|port| (*id, SocketAddr::new(address.ip(), *port)))
                })
                .collect();
            let Some(everyone) = everyone else { continue };
            // A port that changed (its member rebound after a restart): dialed there from now. The
            // publisher takes every node's, this node's own among them: its member's peers leave
            // this node out, so a publisher told only when they changed kept its own node's old
            // port, and a node that led never reached itself (CP-T2.2 step 6's simulation, seed 9).
            if let Some(Slot::Open(seat)) = &mut held.slot {
                seat.peers_changed(&everyone, now);
                held.publisher.set_addresses(&everyone, now);
                continue;
            }
            if let Some(Slot::Bound(_)) = &held.slot
                && let Some(Slot::Bound(driver)) = held.slot.take()
            {
                let plan = Plan {
                    id: self.config.id,
                    peers: everyone
                        .iter()
                        .filter(|(id, _)| *id != self.config.id)
                        .copied()
                        .collect(),
                    data: held.dir.clone(),
                    key_file: self
                        .config
                        .data
                        .join(KEYS_DIR)
                        .join(format!("{incarnation}.key")),
                    memory: self.config.part_memory,
                    members: self.config.ids(),
                    clients: self.ids.nodes(),
                    lane_window: self.ids.lane_window()?,
                    publishers: self.ids.publishers(),
                    #[cfg(feature = "sim")]
                    machine: machine.clone(),
                };
                held.slot = Some(Slot::Open(Box::new(Seat::open(*driver, plan, now)?)));
                held.publisher.set_nodes(everyone, now);
            }
        }
        for record in announce {
            let mut bytes = Vec::new();
            record.encode(&mut bytes);
            let ticket = self.ticket(Waiter::Serves);
            let at_us = (self.clock)();
            self.catalog_publisher.queue(bytes, at_us, ticket, now);
        }
        Ok(())
    }

    /// Feeds each open topic's view what its member's part took, answering this node's publishes
    /// as their batches are applied here.
    fn feed_topics(&mut self) {
        let mut answers: Vec<(u64, Result<Offset, Refusal>)> = Vec::new();
        for held in self.topics.values_mut() {
            let Held {
                name,
                cursor,
                view,
                publisher,
                slot,
                ..
            } = held;
            let Some(Slot::Open(seat)) = slot else {
                continue;
            };
            let name = name.as_str();
            let mut skipped = None;
            let ours = publisher.producer;
            let mut applied: Vec<(u32, Offset)> = Vec::new();
            self.more |= cursor.feed(seat.part(), |fed| match fed {
                Fed::Skip(floor) => {
                    view.skip_to(floor);
                    skipped = Some(floor);
                }
                Fed::Batch(batch) => {
                    let base = batch.base_offset();
                    if view.bounds().1 < base {
                        view.skip_to(base);
                    }
                    for (at, record) in batch.records().enumerate() {
                        let offset = base.saturating_add(u64::try_from(at).unwrap_or(u64::MAX));
                        if offset < view.bounds().1 {
                            continue;
                        }
                        let kept = record.ok().and_then(|r| parse_record(r.payload)).map(
                            |(key, payload)| view.publish(name, key, payload, batch.timestamp()),
                        );
                        if !matches!(kept, Some(Ok(o)) if o == offset) {
                            // A committed message the view can't keep as it was published (Quiver's
                            // fault): its offset is passed over, its readers told they missed it.
                            view.skip_to(offset.saturating_add(1));
                        }
                    }
                    if batch.producer() == ours {
                        applied.push((batch.sequence(), base));
                    }
                }
            });
            if let Some(floor) = skipped {
                answers.extend(
                    publisher
                        .below(floor)
                        .into_iter()
                        .map(|(t, base)| (t, Ok(base))),
                );
            }
            for (sequence, base) in applied {
                if let Some(ticket) = publisher.committed(sequence) {
                    answers.push((ticket, Ok(base)));
                }
            }
        }
        for (ticket, outcome) in answers {
            self.answer(ticket, outcome, None);
        }
    }

    /// The node's commands, as many as wait.
    fn take_commands(&mut self, now: Instant) {
        for command in self.shared.take_commands() {
            self.command(command, now);
        }
    }

    fn command(&mut self, command: Command, now: Instant) {
        {
            match command {
                Command::Publish {
                    name,
                    record,
                    at_us,
                    reply,
                } => {
                    let Some(entry) = self.catalog.get(&name) else {
                        let _ = reply.send(Err(Refusal::NoTopic(name)));
                        return;
                    };
                    if self.publishes >= self.config.publishes {
                        let _ = reply.send(Err(Refusal::Full {
                            what: "the node's persisted publishes waiting",
                            limit: u64::try_from(self.config.publishes).unwrap_or(u64::MAX),
                        }));
                        return;
                    }
                    let incarnation = entry.incarnation;
                    let ticket = self.ticket(Waiter::Publish { reply });
                    self.publishes = self.publishes.saturating_add(1);
                    match self.topics.get_mut(&incarnation) {
                        Some(held) => held.publisher.queue(record, at_us, ticket, now),
                        None => self.answer(ticket, Err(Refusal::NoTopic(name)), None),
                    }
                }
                Command::Create { name, topic, reply } => {
                    let mut bytes = Vec::new();
                    Record::Create {
                        name: name.clone(),
                        topic,
                    }
                    .encode(&mut bytes);
                    let ticket = self.ticket(Waiter::Create { name, reply });
                    let at_us = (self.clock)();
                    self.catalog_publisher.queue(bytes, at_us, ticket, now);
                }
                Command::Delete { name, reply } => {
                    let mut bytes = Vec::new();
                    Record::Delete { name: name.clone() }.encode(&mut bytes);
                    let ticket = self.ticket(Waiter::Delete { name, reply });
                    let at_us = (self.clock)();
                    self.catalog_publisher.queue(bytes, at_us, ticket, now);
                }
                Command::Mark { reply } => {
                    let mut bytes = Vec::new();
                    Record::Mark {
                        node: self.config.id,
                        run: self.run,
                    }
                    .encode(&mut bytes);
                    let ticket = self.ticket(Waiter::Mark { reply });
                    let at_us = (self.clock)();
                    self.catalog_publisher.queue(bytes, at_us, ticket, now);
                }
            }
        }
    }

    /// Every publisher sends what's due; what never could be sent for want of a leader is refused.
    fn pump(&mut self, now: Instant) {
        let wait = self.config.quorum_wait;
        let mut refusals: Vec<(u64, Refusal)> = Vec::new();
        let no_quorum = |what: &str| {
            Refusal::NoQuorum(format!(
                "no leader of {what}'s group was known for {wait:?}"
            ))
        };
        for (ticket, why) in self.catalog_publisher.pump(now) {
            refusals.push((ticket, Refusal::Unavailable(why)));
        }
        for ticket in self.catalog_publisher.expire(wait, now) {
            refusals.push((ticket, no_quorum("the catalog")));
        }

        for held in self.topics.values_mut() {
            for (ticket, why) in held.publisher.pump(now) {
                refusals.push((ticket, Refusal::Unavailable(why)));
            }
            for ticket in held.publisher.expire(wait, now) {
                refusals.push((ticket, no_quorum(&held.name)));
            }
        }
        for (ticket, refusal) in refusals {
            self.answer(ticket, Err(refusal), None);
        }
    }

    /// The node's figures, for its owner.
    fn figures(&mut self) {
        let catalog = GroupFigures::of(&self.catalog_seat, &self.catalog_publisher, 0);
        let topics = self
            .topics
            .iter()
            .map(|(incarnation, held)| {
                let figures = match &held.slot {
                    Some(Slot::Open(seat)) => GroupFigures::of(seat, &held.publisher, *incarnation),
                    _ => GroupFigures {
                        incarnation: *incarnation,
                        ..GroupFigures::default()
                    },
                };
                (held.name.clone(), figures)
            })
            .collect();
        self.shared.set_figures(self.run, catalog, topics);
    }
}

impl Host {
    /// The seed of the next endpoint this start makes: the simulation's, in the order made.
    #[cfg(feature = "sim")]
    fn endpoint_seed(&mut self) -> Option<[u8; 32]> {
        self.sim.as_mut().map(super::sim::Start::endpoint_seed)
    }

    /// None: a node that runs for real seeds its endpoints from the system.
    #[cfg(not(feature = "sim"))]
    #[allow(
        clippy::unused_self,
        reason = "the simulation's build seeds from its start"
    )]
    const fn endpoint_seed(&self) -> Option<[u8; 32]> {
        None
    }

    /// Every open member of the node: the catalog's, then each topic's in the order created.
    #[cfg(feature = "sim")]
    fn seats(&self) -> impl Iterator<Item = &Seat> {
        std::iter::once(&self.catalog_seat).chain(self.topics.values().filter_map(|held| {
            match &held.slot {
                Some(Slot::Open(seat)) => Some(&**seat),
                _ => None,
            }
        }))
    }

    /// Under the simulation: what the members wrote to their logs taken by the node's disk at the
    /// virtual time it drew for it: a cut of each log, each waited for on the shard, so every
    /// answer reaches its member at an instant the seed chose (`docs/design/persisted-sim.md`,
    /// "Determinism"). A member's reads of its log are blocking asks, answered as asked. The
    /// start's kill may end it at a cut.
    #[cfg(feature = "sim")]
    fn settle_logs(&mut self, now: Instant) -> Result<(), Fault> {
        use super::sim::Fate;
        loop {
            if self.sim.is_none() {
                return Ok(());
            }
            // A member's reads of its log are its own blocking asks, answered as asked; what it
            // writes waits for the node's disk.
            let writes = self.seats().fold(0usize, |w, seat| {
                w.saturating_add(seat.member.in_flight().0)
            });
            let Some(sim) = self.sim.as_mut() else {
                return Ok(());
            };
            if writes == 0 {
                sim.next_cut = None;
                return Ok(());
            }
            let due = match sim.next_cut {
                Some(at) => at,
                None => {
                    let at = now
                        .checked_add(sim.latency(hyper_rt::futures::now_ns()))
                        .unwrap_or(now);
                    sim.next_cut = Some(at);
                    at
                }
            };
            if due > now {
                return Ok(());
            }
            sim.next_cut = None;
            match sim.cutting() {
                Fate::Die => return Err(self.die()),
                Fate::DieDuring(ops) => {
                    sim.arm(ops);
                    self.cut_all()?;
                    return Err(self.die());
                }
                Fate::DieAfter => {
                    self.cut_all()?;
                    return Err(self.die());
                }
                Fate::Live => {
                    self.cut_all()?;
                    self.turn_once(Vec::new(), now, now)?;
                }
            }
        }
    }

    /// A cut of every log that holds writes, each waited for: what it held is durable and
    /// answered, or refused where power was cut.
    #[cfg(feature = "sim")]
    fn cut_all(&self) -> Result<(), Fault> {
        for seat in self.seats() {
            if seat.member.in_flight().0 > 0 {
                let cut = seat.member.cut().map_err(|e| e.to_string())?;
                // A cut refused for a power cut is the member's to hear at its turn.
                answered(|| cut.poll().map(|_| ()), || seat.stuck("a cut"))?;
            }
        }
        Ok(())
    }

    /// The start's kill: power cut on every device at once, so nothing the dying node's logs
    /// would write as they close lands, and the host ends; why, and at which await or cut.
    #[cfg(feature = "sim")]
    fn die(&self) -> Fault {
        let Some(sim) = &self.sim else {
            return "killed by the simulation".to_owned();
        };
        sim.arm(0);
        format!("killed by the simulation at {:?}", sim.kill())
    }
}

/// Under the simulation, how long the host waits for its log to answer a cut or a serve, in real
/// time: past it the run is said to hang, never waited on for good.
#[cfg(feature = "sim")]
const HANG: Duration = Duration::from_secs(60);

/// Waits on the shard until `poll` has an answer, at most [`HANG`]: past it, the fault `stuck`
/// says what never answered.
#[cfg(feature = "sim")]
#[allow(
    clippy::disallowed_methods,
    reason = "the simulation's watch for a log that never answers: real time, outside the run's"
)]
fn answered(
    mut poll: impl FnMut() -> Option<()>,
    stuck: impl FnOnce() -> Fault,
) -> Result<(), Fault> {
    let began = Instant::now();
    let mut spins = 0_u32;
    loop {
        if poll().is_some() {
            return Ok(());
        }
        if began.elapsed() > HANG {
            return Err(stuck());
        }
        spins = spins.saturating_add(1);
        if spins < 1_000 {
            std::thread::yield_now();
        } else {
            std::thread::sleep(Duration::from_micros(100));
        }
    }
}

/// The fault every waiting command is answered with once the host ends.
pub(crate) fn ended(why: &str) -> Refusal {
    Refusal::Unavailable(format!("the node's persisted host ended: {why}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Two nodes of a fresh cluster both start at run 1: do take each node's session in each run,
    /// expect no two alike (they were once the run alone, and a node took another's committed
    /// batch for its own), and a node or run past the session's bits refused.
    #[test]
    fn no_two_nodes_or_runs_share_a_session() {
        let mut seen = std::collections::BTreeSet::new();
        for id in 1..=3 {
            for run in 1..=3 {
                assert!(
                    seen.insert(producer(id, run).unwrap()),
                    "node {id} run {run}"
                );
            }
        }
        assert!(producer(1 << 16, 1).is_err());
        assert!(producer(1, 1 << 48).is_err());
    }
}
