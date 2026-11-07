//! The bus kiln's events ride (docs/bus.md): Quiver, vendored at vendor/quiver, its node in the
//! server's process, behind kiln-seams' [`Bus`]. Quiver keeps each topic's messages in memory (the
//! ephemeral class, Focal's doc 33), each a Focal batch charged to one memory budget (`[bus]
//! memory`), and a reader behind what a topic keeps is refused `Behind`, never skipped silently.
//! The live feed of every session rides it (`crate::harness::live`). Opened as a node of a
//! cluster ([`QuiverBus::open_persisted`]), it serves the persisted class too (CP-T2.2): each topic
//! a part of Focal's replicated by the cluster's nodes, a publish acknowledged once a majority's
//! disks hold it; on one server, a group of one, durable on its disk. Other classes are refused,
//! typed, until their goals land. The server opens it so ([`QuiverBus::open_server`]): kiln's CI
//! and deploy events ride a persisted topic (CP-T2.2 step 8).

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use kiln_seams::bus::{Batch, Bus, Class, Message, Offset, Refusal, Topic};
use quiver_api::Topics as _;
use serde::{Deserialize, Serialize};

/// `[bus]` in kiln.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// The bytes the bus's messages may take, their framing included, until every reader has let
    /// them go. Past it a topic lets its oldest go to take a new message; one it can't make room
    /// for is refused, typed.
    pub memory: u64,
    /// The bytes one topic keeps at most (its keys and payloads), beside the messages it retains.
    pub topic_bytes: u64,
    /// Where the server keeps the persisted class: its topics' logs and parts, and the identities
    /// its node and publishers speak as. Empty: the data directory's `bus`.
    pub data: String,
    /// Each persisted part's memory: what its window keeps of its messages, and so what its topic
    /// holds again after a restart, its log on disk about twice it.
    pub part_memory: u64,
    /// The bytes a persisted topic keeps for its readers (its keys and payloads): kiln's events
    /// topic, what a delivery target away the longest may still read.
    pub persisted_bytes: u64,
}

/// Room for one more of Quiver's largest messages with its framing: half the least a node takes
/// (`quiver::NodeConfig::MEMORY_MIN`, two of them and a segment), 1,083,392 bytes.
const LARGEST: u64 = (quiver::NodeConfig::MEMORY_MIN / 2) as u64;

/// The default `[bus] memory`: 256 MiB, Quiver's own as CP-T2.1 set it (d06090634d8). Goal 11 cut it to the bus
/// lab's 171 MiB (b167fb82145), a ceiling the lab never reached. Quiver's budget is a counter, so the ceiling costs
/// nothing at rest, and lowering it could only cost Quiver room it would use (a topic refused, its viewers starting
/// over): Ada's rule is that no change costs Quiver's performance (goal 11's audit, row C10). What the bus can hold
/// follows the topics it holds anyway ([`QuiverBus::bound`]).
pub const MEMORY: u64 = 256 << 20;
/// The default `[bus] topic_bytes`: 16 MiB, Quiver's own as CP-T2.1 set it, as [`MEMORY`] is: goal 11 cut it to 5
/// MiB, past which a watched session's oldest frames go and its slow viewers fall behind.
pub const TOPIC_BYTES: u64 = 16 << 20;

/// The most Quiver charges a message past its key and payload, 86 bytes:
/// - its Focal batch's header: 56 bytes (`focal_bus::batch::HEADER`);
/// - its record's head there: its length's and delta's LEB128s, 20 bytes at the most (`RECORD_HEAD_MAX`);
/// - Quiver's own record head: the key's length as a LEB128, 10 bytes at the most.
///
/// `g11_s3_a_message_costs_the_bus_its_bytes_and_its_framing_at_most` measures it against the node.
const FRAMING: u64 = 56 + 20 + 10;

/// The default `[bus] part_memory`: 64 MiB, a persisted topic's bytes again, so a restart holds
/// what the topic kept (docs/bus.md, "The persisted class").
pub const PART_MEMORY: u64 = 64 << 20;
/// The default `[bus] persisted_bytes`: 64 MiB, days of kiln's events (docs/bus.md, "The persisted
/// class").
pub const PERSISTED_BYTES: u64 = 64 << 20;
/// Persisted topics the server's cluster holds at once: kiln's events, with room.
pub const PERSISTED_TOPICS: usize = 16;
/// Publishes the server's node holds at once, waiting to be committed.
pub const PERSISTED_PUBLISHES: usize = 1024;
/// How long a persisted publish waits for its group's leader (on one server, itself once elected)
/// before it's refused for no quorum.
pub const QUORUM_WAIT: Duration = Duration::from_secs(10);
/// Topics the bus holds at once: a session's live feed is one.
pub const TOPICS: usize = 65_536;
/// Subscriptions the bus holds at once.
pub const SUBSCRIPTIONS: usize = 65_536;

impl Default for Config {
    fn default() -> Self {
        Self { memory: MEMORY, topic_bytes: TOPIC_BYTES, data: String::new(), part_memory: PART_MEMORY, persisted_bytes: PERSISTED_BYTES }
    }
}

impl Config {
    /// Where the persisted class keeps its data: `data`, else `data_dir`'s `bus`.
    pub fn data_in(&self, data_dir: &Path) -> PathBuf {
        if self.data.trim().is_empty() { data_dir.join("bus") } else { PathBuf::from(&self.data) }
    }

    /// The persisted class as kiln's server runs it, its data in `dir`: the one node of a cluster
    /// of one, on loopback at a port the system picks (no peer dials it), its identities made
    /// there the first time and sealed under a key file beside them.
    pub fn persisted(&self, dir: &Path) -> quiver::PersistedConfig {
        quiver::PersistedConfig {
            id: 1,
            nodes: vec![(1, std::net::SocketAddr::from(([127, 0, 0, 1], 0)))],
            data: dir.join("node"),
            identities: dir.join("identities"),
            identities_key: Some(dir.join("identities.key")),
            part_memory: self.part_memory,
            topics: PERSISTED_TOPICS,
            publishes: PERSISTED_PUBLISHES,
            quorum_wait: QUORUM_WAIT,
        }
    }
}

/// Quiver's node as kiln-seams' [`Bus`].
#[derive(Clone)]
pub struct QuiverBus {
    node: quiver::Node,
    topic_bytes: u64,
    /// The most messages any topic made on it retains: what its topics' framing is reckoned for ([`QuiverBus::bound`]).
    retain_most: Arc<AtomicU64>,
    persisted_bytes: u64,
    /// Why the persisted class didn't open, for a server's bus that serves the ephemeral class
    /// alone.
    refused: Option<Arc<str>>,
}

impl QuiverBus {
    /// The node `cfg` says, stamping messages by `clock`.
    pub fn open(cfg: &Config, clock: Arc<dyn kiln_seams::Clock>) -> anyhow::Result<Self> {
        Self::open_with(cfg, clock, None)
    }

    /// The node `cfg` says as node `persisted.id` of the cluster `persisted` names, serving the
    /// persisted class too, stamping messages by `clock`.
    pub fn open_persisted(cfg: &Config, persisted: quiver::PersistedConfig, clock: Arc<dyn kiln_seams::Clock>) -> anyhow::Result<Self> {
        Self::open_with(cfg, clock, Some(persisted))
    }

    /// The server's bus (CP-T2.2 step 8): the ephemeral class for its sessions' live feeds, and the
    /// persisted class as the one node of a cluster of one ([`Config::persisted`]), its data in
    /// `dir`, stamping messages by `clock`. A persisted class that can't open leaves the bus with
    /// the ephemeral class alone, saying why ([`QuiverBus::persisted_refused`]), so a damaged bus
    /// directory stops kiln's events, which wait where they were made, not the server.
    pub fn open_server(cfg: &Config, dir: &Path, clock: Arc<dyn kiln_seams::Clock>) -> anyhow::Result<Self> {
        match Self::open_persisted(cfg, cfg.persisted(dir), Arc::clone(&clock)) {
            Ok(bus) => Ok(bus),
            Err(e) => {
                let why = format!("{e:#}");
                tracing::error!(target: "kiln.bus", event = "persisted_refused", dir = %dir.display(), error = %why, "the bus's persisted class didn't open: kiln's events wait in its outbox until it does");
                Ok(Self { refused: Some(Arc::from(why)), ..Self::open(cfg, clock)? })
            }
        }
    }

    fn open_with(cfg: &Config, clock: Arc<dyn kiln_seams::Clock>, persisted: Option<quiver::PersistedConfig>) -> anyhow::Result<Self> {
        let memory = usize::try_from(cfg.memory).map_err(|_| anyhow::anyhow!("[bus] memory: {} bytes is more than this machine addresses", cfg.memory))?;
        for (name, bytes) in [("topic_bytes", cfg.topic_bytes), ("persisted_bytes", cfg.persisted_bytes)] {
            anyhow::ensure!((1..=quiver_api::RETAIN_BYTES_MAX).contains(&bytes), "[bus] {name}: {bytes} (1 to {} bytes)", quiver_api::RETAIN_BYTES_MAX);
        }
        let stamp: quiver::Clock = Arc::new(move || u64::try_from(clock.unix_ms()).unwrap_or(0).saturating_mul(1000));
        let node = quiver::Node::with(quiver::NodeConfig { memory, topics: TOPICS, subscriptions: SUBSCRIPTIONS }, stamp, persisted).map_err(|e| anyhow::anyhow!("[bus]: {e}"))?;
        Ok(Self { node, topic_bytes: cfg.topic_bytes, retain_most: Arc::new(AtomicU64::new(0)), persisted_bytes: cfg.persisted_bytes, refused: None })
    }

    /// Why the server's bus serves no persisted class, if it doesn't: its persisted class failed
    /// to open ([`QuiverBus::open_server`]).
    pub fn persisted_refused(&self) -> Option<&str> {
        self.refused.as_deref()
    }

    /// The persisted class's figures, for a bus that serves it: whether it has caught up with its
    /// catalog, each topic's group.
    pub fn persisted_stats(&self) -> Option<quiver::PersistedStats> {
        self.node.persisted_stats()
    }

    /// Stops the persisted class at once, as a crash would (a test's kill -9, in process).
    pub fn halt_persisted(&self) {
        self.node.halt_persisted();
    }

    /// Its node's figures now: topics, bytes held, lag notices.
    pub fn stats(&self) -> quiver::Stats {
        self.node.stats()
    }

    /// The most the bus can hold with what `stats` says it holds (goal 11, plan 01a1a87b2e56 step 3). It is on
    /// `/metrics` as `kiln_memory_bound_bytes{owner="bus"}`, and is never more than its limit (`[bus] memory`).
    /// For each topic it holds, it counts:
    /// - the messages the topic keeps: their keys and payloads, at most `[bus] topic_bytes`, each with its
    ///   framing ([`FRAMING`]), at most as many as the most any topic retains;
    /// - the messages of its oldest segment let go while that segment still holds kept ones: a segment of two or
    ///   more messages is at most `quiver::ephemeral::SEGMENT_BYTES`;
    /// - a message being taken before the oldest go ([`LARGEST`]).
    ///
    /// For each subscription, it counts the segment the subscription reads and a message: one that stalls keeps
    /// them alive. So a server holding few topics is bound by those, not by the most a large one holds.
    pub fn bound(&self, stats: &quiver::Stats) -> u64 {
        let wide = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
        let segment = wide(quiver::ephemeral::SEGMENT_BYTES);
        let framing = self.retain_most.load(Ordering::Relaxed).saturating_mul(FRAMING);
        let topic = self.topic_bytes.saturating_add(framing).saturating_add(segment).saturating_add(LARGEST);
        let subscription = segment.saturating_add(LARGEST);
        wide(stats.topics).saturating_mul(topic).saturating_add(wide(stats.subscriptions).saturating_mul(subscription)).min(wide(stats.memory_limit))
    }

    /// Sets the `kiln_bus_*` gauges from its node's figures (a scrape asks for them), and the bound that binds now
    /// ([`QuiverBus::bound`]).
    pub fn observe(&self) {
        let s = self.node.stats();
        let g = crate::metrics::bus_gauges();
        let wide = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
        g.topics.set(wide(s.topics));
        g.memory_bytes.set(wide(s.memory_used));
        g.memory_limit_bytes.set(wide(s.memory_limit));
        g.memory_bound_bytes.set(i64::try_from(self.bound(&s)).unwrap_or(i64::MAX));
    }
}

fn class(c: Class) -> quiver_api::Class {
    match c {
        Class::Ephemeral => quiver_api::Class::Ephemeral,
        Class::Replicated => quiver_api::Class::Replicated,
        Class::Persisted => quiver_api::Class::Persisted,
        Class::Windowed => quiver_api::Class::Windowed,
    }
}

/// Quiver's refusal as the seam's: each typed, none dropped.
fn refusal(r: quiver_api::Refusal) -> Refusal {
    let wide = |n: u64| usize::try_from(n).unwrap_or(usize::MAX);
    match r {
        quiver_api::Refusal::NoTopic(name) => Refusal::NoTopic(name),
        quiver_api::Refusal::Exists(name) => Refusal::Exists(name),
        quiver_api::Refusal::Invalid { what, why } => Refusal::Invalid { what, why },
        quiver_api::Refusal::TooLarge { what, size, limit } => Refusal::TooLarge { what, size: wide(size), limit: wide(limit) },
        quiver_api::Refusal::Full { what: "the node's topics", limit } => Refusal::Full { limit: wide(limit) },
        quiver_api::Refusal::Full { what, limit } => Refusal::Unavailable(format!("{what} is at its bound ({limit})")),
        quiver_api::Refusal::Behind { asked, first } => Refusal::Behind { asked, first },
        quiver_api::Refusal::Unsupported(c) => Refusal::Unavailable(format!("Quiver doesn't serve the {} class yet", c.name())),
        quiver_api::Refusal::Unavailable(why) => Refusal::Unavailable(why),
        quiver_api::Refusal::NoQuorum(why) => Refusal::Unavailable(format!("no quorum: {why}")),
    }
}

impl Bus for QuiverBus {
    fn create<'a>(&'a self, name: &'a str, topic: Topic) -> BoxFuture<'a, Result<(), Refusal>> {
        let retain_messages = u64::try_from(topic.retain).unwrap_or(u64::MAX);
        let retain_bytes = if topic.class == Class::Persisted { self.persisted_bytes } else { self.topic_bytes };
        let quiver_topic = quiver_api::Topic { class: class(topic.class), retain_messages, retain_bytes };
        Box::pin(async move {
            self.node.create(name, quiver_topic).await.map_err(refusal)?;
            // Raised once it's made, before anything is published to it: the bound counts its framing from then on.
            self.retain_most.fetch_max(retain_messages, Ordering::Relaxed);
            Ok(())
        })
    }

    fn publish<'a>(&'a self, name: &'a str, key: Option<Vec<u8>>, payload: Vec<u8>) -> BoxFuture<'a, Result<Offset, Refusal>> {
        Box::pin(async move { self.node.publish(name, key.as_deref(), &payload).await.map_err(refusal) })
    }

    fn read<'a>(&'a self, name: &'a str, from: Offset, limit: usize) -> BoxFuture<'a, Result<Batch, Refusal>> {
        Box::pin(async move {
            let batch = self.node.read(name, from, limit).await.map_err(refusal)?;
            let messages = batch
                .messages
                .into_iter()
                .map(|m| Message { offset: m.offset, key: m.key, payload: m.payload, at_ms: i64::try_from(m.at_us / 1000).unwrap_or(i64::MAX) })
                .collect();
            Ok(Batch { messages, next: batch.next })
        })
    }

    fn wait<'a>(&'a self, name: &'a str, from: Offset) -> BoxFuture<'a, Result<(), Refusal>> {
        Box::pin(async move { self.node.wait(name, from).await.map_err(refusal) })
    }

    fn bounds<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(Offset, Offset), Refusal>> {
        Box::pin(async move { self.node.bounds(name).await.map_err(refusal) })
    }

    fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        Box::pin(async move { self.node.delete(name).await.map_err(refusal) })
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::Duration;

    use kiln_seams::bus::{BusFixture, cases};
    use kiln_seams::clock::SimClock;

    use super::*;

    struct QuiverFixture(QuiverBus);

    impl BusFixture for QuiverFixture {
        fn bus(&self) -> &dyn Bus {
            &self.0
        }
    }

    fn quiver(clock: &SimClock) -> anyhow::Result<QuiverBus> {
        QuiverBus::open(&Config::default(), Arc::new(clock.clone()))
    }

    /// CP-T2.1, AC-T2.1.1: do the seam's every clause (BUS-1 to BUS-11) against Quiver's node, a
    /// fresh one a clause, expect every one to pass.
    #[test]
    fn quiver_passes_the_bus_suite() -> anyhow::Result<()> {
        let (passed, failed) = crate::rt::block_on(async {
            let (mut passed, mut failed) = (Vec::new(), Vec::new());
            for case in cases::<QuiverFixture>() {
                let fixture = QuiverFixture(quiver(&SimClock::new())?);
                match crate::rt::timeout(Duration::from_secs(60), (case.check)(&fixture)).await {
                    Ok(Ok(())) => passed.push(case.id),
                    Ok(Err(why)) => failed.push(format!("{}: {why}", case.id)),
                    Err(_) => failed.push(format!("{}: it didn't end in a minute", case.id)),
                }
            }
            Ok((passed, failed))
        })?;
        assert!(failed.is_empty(), "{failed:#?}");
        assert_eq!(passed.len(), cases::<QuiverFixture>().len());
        Ok(())
    }

    /// A persisted bus and the directory its node keeps its data in, gone with it.
    struct PersistedFixture {
        bus: QuiverBus,
        _dir: tempfile::TempDir,
    }

    impl BusFixture for PersistedFixture {
        fn bus(&self) -> &dyn Bus {
            &self.bus
        }

        fn class(&self) -> Class {
            Class::Persisted
        }
    }

    /// Quiver's node as kiln's server runs the persisted class ([`QuiverBus::open_server`]): a
    /// cluster of one, its data in `dir`, its catalog caught up with itself.
    fn persisted(clock: &SimClock, dir: &std::path::Path) -> anyhow::Result<QuiverBus> {
        let cfg = Config { part_memory: quiver::PersistedConfig::PART_MEMORY_MIN, ..Config::default() };
        let bus = QuiverBus::open_server(&cfg, dir, Arc::new(clock.clone()))?;
        anyhow::ensure!(bus.persisted_refused().is_none(), "{:?}", bus.persisted_refused());
        let began = std::time::Instant::now();
        while !bus.persisted_stats().is_some_and(|s| s.settled) {
            anyhow::ensure!(began.elapsed() < Duration::from_secs(60), "the bus never caught up with its catalog");
            std::thread::sleep(Duration::from_millis(2));
        }
        Ok(bus)
    }

    /// CP-T2.2 step 8: a server's bus whose persisted class can't open (its directory a file) still
    /// opens, serving the ephemeral class: do open one so, expect the live feeds' class to work,
    /// a persisted topic refused `Unavailable`, and the bus to say why.
    #[test]
    fn cp_t2_2_a_server_bus_whose_persisted_class_cant_open_serves_the_ephemeral_class() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let blocked = dir.path().join("bus");
        std::fs::write(&blocked, b"not a directory")?;
        let bus = QuiverBus::open_server(&Config::default(), &blocked, Arc::new(SimClock::new()))?;
        assert!(bus.persisted_refused().is_some_and(|why| !why.is_empty()));
        assert!(bus.persisted_stats().is_none());
        futures::executor::block_on(bus.create("live", Topic { class: Class::Ephemeral, retain: 10 }))?;
        assert_eq!(futures::executor::block_on(bus.publish("live", None, b"x".to_vec()))?, 0);
        let refused = futures::executor::block_on(bus.create("kiln.events", Topic { class: Class::Persisted, retain: 10 }));
        assert!(matches!(&refused, Err(Refusal::Unavailable(why)) if why.contains("persisted")), "{refused:?}");
        Ok(())
    }

    /// CP-T2.2 step 8: the server's persisted class is a cluster of one at a port the system picks,
    /// its data where `[bus] data` says, else the data directory's `bus`.
    #[test]
    fn cp_t2_2_the_servers_persisted_class_is_a_cluster_of_one_where_bus_data_says() {
        let cfg = Config::default();
        assert_eq!(cfg.data_in(std::path::Path::new("/srv/kiln")), PathBuf::from("/srv/kiln/bus"));
        let elsewhere = Config { data: "/fast/bus".into(), ..Config::default() };
        assert_eq!(elsewhere.data_in(std::path::Path::new("/srv/kiln")), PathBuf::from("/fast/bus"));
        let p = cfg.persisted(std::path::Path::new("/srv/kiln/bus"));
        assert_eq!((p.id, p.nodes.len()), (1, 1));
        assert!(p.nodes.iter().all(|(_, a)| a.ip().is_loopback() && a.port() == 0));
        assert_eq!(p.data, PathBuf::from("/srv/kiln/bus/node"));
        assert_eq!(p.identities_key, Some(PathBuf::from("/srv/kiln/bus/identities.key")));
        assert_eq!(p.part_memory, PART_MEMORY);
    }

    /// CP-T2.2 step 4: do the seam's every clause (BUS-1 to BUS-11) against Quiver's persisted
    /// class on one server (a group of one, durable on its disk), a fresh node a clause, expect
    /// every one to pass.
    #[test]
    fn cp_t2_2_quivers_persisted_class_passes_the_bus_suite_on_one_server() -> anyhow::Result<()> {
        let (passed, failed) = crate::rt::block_on(async {
            let (mut passed, mut failed) = (Vec::new(), Vec::new());
            for case in cases::<PersistedFixture>() {
                let dir = tempfile::tempdir()?;
                let fixture = PersistedFixture { bus: persisted(&SimClock::new(), dir.path())?, _dir: dir };
                match crate::rt::timeout(Duration::from_secs(60), (case.check)(&fixture)).await {
                    Ok(Ok(())) => passed.push(case.id),
                    Ok(Err(why)) => failed.push(format!("{}: {why}", case.id)),
                    Err(_) => failed.push(format!("{}: it didn't end in a minute", case.id)),
                }
            }
            Ok((passed, failed))
        })?;
        assert!(failed.is_empty(), "{failed:#?}");
        assert_eq!(passed.len(), cases::<PersistedFixture>().len());
        Ok(())
    }

    /// A class Quiver doesn't serve yet is refused, typed, and makes no topic: the replicated
    /// class, and the persisted class on a node of no cluster.
    #[test]
    fn a_class_quiver_doesnt_serve_yet_is_refused_typed() -> anyhow::Result<()> {
        let bus = quiver(&SimClock::new())?;
        let replicated = Topic { class: Class::Replicated, retain: 10 };
        let refused = futures::executor::block_on(bus.create("ci/events", replicated));
        assert!(matches!(&refused, Err(Refusal::Unavailable(why)) if why.contains("replicated")), "{refused:?}");
        let persisted = Topic { class: Class::Persisted, retain: 10 };
        let refused = futures::executor::block_on(bus.create("ci/events", persisted));
        assert!(matches!(&refused, Err(Refusal::Unavailable(why)) if why.contains("persisted")), "{refused:?}");
        assert_eq!(futures::executor::block_on(bus.bounds("ci/events")), Err(Refusal::NoTopic("ci/events".into())));
        Ok(())
    }

    /// Messages carry the clock kiln gives the bus, in milliseconds.
    #[test]
    fn messages_are_stamped_by_the_clock_kiln_gives() -> anyhow::Result<()> {
        let clock = SimClock::new();
        let bus = quiver(&clock)?;
        futures::executor::block_on(bus.create("t", Topic { class: Class::Ephemeral, retain: 10 }))?;
        clock.advance(Duration::from_millis(1500));
        futures::executor::block_on(bus.publish("t", None, b"x".to_vec()))?;
        let read = futures::executor::block_on(bus.read("t", 0, 1))?;
        assert_eq!(read.messages.first().map(|m| m.at_ms), Some(kiln_seams::clock::SIM_EPOCH_MS + 1500));
        Ok(())
    }

    /// A topic keeps at most `[bus] topic_bytes`: past it its oldest go, whatever it retains.
    #[test]
    fn a_topic_keeps_at_most_its_bytes() -> anyhow::Result<()> {
        let bus = QuiverBus::open(&Config { topic_bytes: 10, ..Config::default() }, Arc::new(SimClock::new()))?;
        futures::executor::block_on(bus.create("t", Topic { class: Class::Ephemeral, retain: 100 }))?;
        for p in ["aaaa", "bbbb", "cccc"] {
            futures::executor::block_on(bus.publish("t", None, p.as_bytes().to_vec()))?;
        }
        assert_eq!(futures::executor::block_on(bus.bounds("t"))?, (1, 3));
        Ok(())
    }

    /// Goal 11's audit, row C10: the defaults are Quiver's own as CP-T2.1 set them, the numbers docs/bus.md gives,
    /// and a server takes them as they are. What the bus lab measured (bus6, 2026-11-05: its busiest second
    /// 12,157,694 B for 11 topics, its largest topic 3,672,418 B) is within them, with room for one more of the
    /// largest message a topic takes, where goal 11's 5 MiB left a topic 1.4 MiB past what it measured.
    #[test]
    fn g11_c10_the_bus_defaults_are_quivers_own() -> anyhow::Result<()> {
        anyhow::ensure!((MEMORY, TOPIC_BYTES) == (256 << 20, 16 << 20), "the defaults docs/bus.md gives: {MEMORY}, {TOPIC_BYTES}");
        let bus = quiver(&SimClock::new())?;
        anyhow::ensure!(u64::try_from(bus.stats().memory_limit)? == MEMORY, "a server's bus takes the default");
        futures::executor::block_on(bus.create("feed", Topic { class: Class::Ephemeral, retain: crate::harness::live::RETAIN }))?;
        let at = |topics: usize| bus.bound(&quiver::Stats { topics, memory_limit: usize::try_from(MEMORY).unwrap_or(usize::MAX), ..quiver::Stats::default() });
        anyhow::ensure!(at(11) >= 12_157_694, "11 topics are bound at {}, under the bytes the lab's 11 held", at(11));
        anyhow::ensure!(TOPIC_BYTES >= 3_672_418 + u64::try_from(quiver_api::PAYLOAD_MAX)?, "the lab's largest topic and one more of the largest message");
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: a message costs the bus's memory its key's and payload's bytes and at
    /// most [`FRAMING`] more, whatever its size (each LEB128 of its framing from one byte to three) and whether
    /// or not it has a key. That's what [`QuiverBus::bound`] reckons each message kept at.
    #[test]
    fn g11_s3_a_message_costs_the_bus_its_bytes_and_its_framing_at_most() -> anyhow::Result<()> {
        // Room for every message here, so none is let go while it's measured.
        let bus = QuiverBus::open(&Config { memory: 256 << 20, topic_bytes: 64 << 20, ..Config::default() }, Arc::new(SimClock::new()))?;
        futures::executor::block_on(bus.create("t", Topic { class: Class::Ephemeral, retain: 1000 }))?;
        for size in [0, 1, 127, 128, 16_383, 16_384, 65_536, quiver_api::PAYLOAD_MAX] {
            for key in [None, Some(vec![7; quiver_api::KEY_MAX])] {
                let bytes = u64::try_from(size + key.as_ref().map_or(0, Vec::len))?;
                let before = bus.stats().memory_used;
                futures::executor::block_on(bus.publish("t", key, vec![1; size]))?;
                let cost = u64::try_from(bus.stats().memory_used.saturating_sub(before))?;
                anyhow::ensure!(cost >= bytes && cost <= bytes + FRAMING, "a message of {bytes} bytes cost the bus {cost}");
            }
        }
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: what the bus holds never passes its bound.
    /// - The bound follows the topics it holds: nothing for none, then each topic's most as each is made, its
    ///   messages passing its bytes many times over.
    /// - It falls back when a topic goes.
    /// - The limit binds a bus of many topics.
    #[test]
    fn g11_s3_the_bus_holds_no_more_than_its_bound_and_the_bound_follows_its_topics() -> anyhow::Result<()> {
        let cfg = Config { memory: 64 << 20, topic_bytes: 64 << 10, ..Config::default() };
        let bus = QuiverBus::open(&cfg, Arc::new(SimClock::new()))?;
        let held = |bus: &QuiverBus| -> anyhow::Result<(u64, u64)> {
            let s = bus.stats();
            Ok((u64::try_from(s.memory_used)?, bus.bound(&s)))
        };
        anyhow::ensure!(held(&bus)? == (0, 0), "a bus holding no topic is bound at nothing: {:?}", held(&bus)?);
        let mut bounds = Vec::new();
        for t in 0..3 {
            let name = format!("t{t}");
            futures::executor::block_on(bus.create(&name, Topic { class: Class::Ephemeral, retain: 50 }))?;
            // From a byte to 5 KB: segments fill by their bytes and by their slots, and the topic lets its oldest go
            // by its bytes and by its count.
            for i in 0..400 {
                let size = 1 + i * 37 % 5_000;
                futures::executor::block_on(bus.publish(&name, None, vec![2; size]))?;
                let (used, bound) = held(&bus)?;
                anyhow::ensure!(used <= bound, "topic {t}, message {i}: the bus holds {used}, bound at {bound}");
            }
            bounds.push(held(&bus)?.1);
        }
        anyhow::ensure!(bounds.windows(2).all(|w| matches!(w, [a, b] if a < b)), "each topic adds its most: {bounds:?}");
        futures::executor::block_on(bus.delete("t0"))?;
        let (used, bound) = held(&bus)?;
        anyhow::ensure!(used <= bound && bounds.last().is_some_and(|three| bound < *three), "a topic gone: the bus holds {used}, bound at {bound} ({bounds:?})");
        // At the least a node takes, the limit binds before a few topics' most.
        let small = QuiverBus::open(&Config { memory: u64::try_from(quiver::NodeConfig::MEMORY_MIN)?, topic_bytes: cfg.topic_bytes, ..Config::default() }, Arc::new(SimClock::new()))?;
        for t in 0..8 {
            futures::executor::block_on(small.create(&format!("t{t}"), Topic { class: Class::Ephemeral, retain: 50 }))?;
        }
        let s = small.stats();
        anyhow::ensure!(small.bound(&s) == u64::try_from(s.memory_limit)?, "8 topics past the limit are bound by it: {}", small.bound(&s));
        Ok(())
    }

    /// CP-T2.1: docs/bus.md says every `[bus]` setting the code reads, with its default.
    #[test]
    fn docs_bus_md_says_every_setting_and_its_default() -> anyhow::Result<()> {
        let doc = std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/bus.md"))?;
        let cfg = toml::Value::try_from(Config::default())?;
        let settings: Vec<(String, String)> = cfg.as_table().into_iter().flatten().map(|(k, v)| (k.clone(), v.to_string())).collect();
        assert_eq!(settings.len(), 5, "{settings:?}");
        for (key, default) in settings {
            assert!(doc.contains(&format!("| `{key}`")), "docs/bus.md doesn't name [bus] {key}");
            assert!(doc.contains(&format!("`{key} = {default}`")), "docs/bus.md doesn't give [bus] {key}'s default as `{key} = {default}`");
        }
        Ok(())
    }
}
