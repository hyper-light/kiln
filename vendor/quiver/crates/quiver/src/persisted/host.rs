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
use hyper_rt_transport::Driver;
use hyper_transport::Event;
use quiver_api::{Offset, Refusal, Topic};

use super::catalog::{Applied, Catalog, Record};
use super::group::{
    Cursor, Drv, Fed, Identities, Plan, Publisher, STREAM, Seat, Settled, drop_event,
};
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

fn bind(ids: &Identities, id: u64, address: SocketAddr, member: bool) -> Result<Drv, Fault> {
    let endpoint = if member {
        ids.member_endpoint(id)?
    } else {
        ids.client_endpoint(id)?
    };
    Driver::bind(endpoint, address, focal_bus::limits::IO)
        .map_err(|e| format!("{address} could not be bound: {e:?}"))
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
    ) -> Result<Self, Fault> {
        let now = Instant::now();
        make_dir(&config.data)?;
        let run = raise_run(&config.data.join(RUN_FILE))?;
        let ids = Identities::load(&config.identities, config.nodes.len())?;
        let own = config
            .address(config.id)
            .ok_or("this node isn't among the cluster's nodes")?;
        let catalog_dir = config.data.join(CATALOG_DIR);
        make_dir(&catalog_dir)?;
        let keys = config.data.join(KEYS_DIR);
        make_dir(&keys)?;
        let member_driver = bind(&ids, config.id, own, true)?;
        let plan = Plan {
            id: config.id,
            peers: config.peers(),
            data: catalog_dir,
            key_file: keys.join("catalog.key"),
            memory: config.part_memory,
            members: config.ids(),
            clients: ids.nodes(),
            lane_window: ids.lane_window()?,
        };
        let catalog_seat = Seat::open(member_driver, plan, now)?;
        let client_driver = bind(&ids, config.id, SocketAddr::new(own.ip(), 0), false)?;
        let mut catalog_publisher =
            Publisher::new(client_driver, producer(config.id, run)?, config.id, now);
        catalog_publisher.set_nodes(config.nodes.clone(), now);
        let mut start = Vec::new();
        Record::Start {
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
        })
    }

    /// Runs until the node is told to stop, or a fault it can't go past.
    pub(crate) async fn run(&mut self) -> Result<(), Fault> {
        loop {
            if self.shared.stopping() {
                return Ok(());
            }
            let began = Instant::now();
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
            let now = Instant::now();
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
        all.into_iter().flatten().min()
    }

    fn turn(
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
        Ok(())
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
        let driver = bind(
            &self.ids,
            self.config.id,
            SocketAddr::new(own.ip(), 0),
            false,
        )?;
        let publisher = Publisher::new(
            driver,
            producer(self.config.id, self.run)?,
            self.config.id,
            Instant::now(),
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
        for (incarnation, held) in &mut self.topics {
            if held.slot.is_none() {
                make_dir(&held.dir)?;
                let kept = kept_port(&held.dir)?;
                let at = |port| SocketAddr::new(own.ip(), port);
                // The port it kept, unless another socket took it while the node was down: then
                // one the system picks, recorded in the catalog as the part's port at this node.
                let driver = match kept.map(|port| bind(&self.ids, self.config.id, at(port), true))
                {
                    Some(Ok(driver)) => driver,
                    _ => bind(&self.ids, self.config.id, at(0), true)?,
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
            // A peer's port that changed (it rebound after a restart): dialed there from now.
            if let Some(Slot::Open(seat)) = &mut held.slot {
                if seat.peers_changed(&everyone) {
                    held.publisher.set_addresses(&everyone);
                }
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
