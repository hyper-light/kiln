//! The bus kiln's events ride (docs/bus.md): Quiver, vendored at vendor/quiver, its node in the
//! server's process, behind kiln-seams' [`Bus`]. Quiver keeps each topic's messages in memory (the
//! ephemeral class, Focal's doc 33), each a Focal batch charged to one memory budget (`[bus]
//! memory`), and a reader behind what a topic keeps is refused `Behind`, never skipped silently.
//! The live feed of every session rides it (`crate::harness::live`). Classes other than ephemeral
//! are refused, typed, until their goals land (CP-T2.2 on).

use std::sync::Arc;

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
}

/// The default `[bus] memory`: 256 MiB.
pub const MEMORY: u64 = 256 << 20;
/// The default `[bus] topic_bytes`: 16 MiB.
pub const TOPIC_BYTES: u64 = 16 << 20;
/// Topics the bus holds at once: a session's live feed is one.
pub const TOPICS: usize = 65_536;
/// Subscriptions the bus holds at once.
pub const SUBSCRIPTIONS: usize = 65_536;

impl Default for Config {
    fn default() -> Self {
        Self { memory: MEMORY, topic_bytes: TOPIC_BYTES }
    }
}

/// Quiver's node as kiln-seams' [`Bus`].
#[derive(Clone)]
pub struct QuiverBus {
    node: quiver::Node,
    topic_bytes: u64,
}

impl QuiverBus {
    /// The node `cfg` says, stamping messages by `clock`.
    pub fn open(cfg: &Config, clock: Arc<dyn kiln_seams::Clock>) -> anyhow::Result<Self> {
        let memory = usize::try_from(cfg.memory).map_err(|_| anyhow::anyhow!("[bus] memory: {} bytes is more than this machine addresses", cfg.memory))?;
        anyhow::ensure!(
            cfg.topic_bytes >= 1 && cfg.topic_bytes <= quiver_api::RETAIN_BYTES_MAX,
            "[bus] topic_bytes: {} (1 to {} bytes)",
            cfg.topic_bytes,
            quiver_api::RETAIN_BYTES_MAX
        );
        let stamp: quiver::Clock = Arc::new(move || u64::try_from(clock.unix_ms()).unwrap_or(0).saturating_mul(1000));
        let node = quiver::Node::with_clock(quiver::NodeConfig { memory, topics: TOPICS, subscriptions: SUBSCRIPTIONS }, stamp).map_err(|e| anyhow::anyhow!("[bus]: {e}"))?;
        Ok(Self { node, topic_bytes: cfg.topic_bytes })
    }

    /// Its node's figures now: topics, bytes held, lag notices.
    pub fn stats(&self) -> quiver::Stats {
        self.node.stats()
    }

    /// Sets the `kiln_bus_*` gauges from its node's figures (a scrape asks for them).
    pub fn observe(&self) {
        let s = self.node.stats();
        let g = crate::metrics::bus_gauges();
        let wide = |n: usize| i64::try_from(n).unwrap_or(i64::MAX);
        g.topics.set(wide(s.topics));
        g.memory_bytes.set(wide(s.memory_used));
        g.memory_limit_bytes.set(wide(s.memory_limit));
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
    }
}

impl Bus for QuiverBus {
    fn create<'a>(&'a self, name: &'a str, topic: Topic) -> BoxFuture<'a, Result<(), Refusal>> {
        let retain_messages = u64::try_from(topic.retain).unwrap_or(u64::MAX);
        let quiver_topic = quiver_api::Topic { class: class(topic.class), retain_messages, retain_bytes: self.topic_bytes };
        Box::pin(async move { self.node.create(name, quiver_topic).await.map_err(refusal) })
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

    /// A class Quiver doesn't serve yet is refused, typed, and makes no topic.
    #[test]
    fn a_class_quiver_doesnt_serve_yet_is_refused_typed() -> anyhow::Result<()> {
        let bus = quiver(&SimClock::new())?;
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

    /// CP-T2.1: docs/bus.md says every `[bus]` setting the code reads, with its default.
    #[test]
    fn docs_bus_md_says_every_setting_and_its_default() -> anyhow::Result<()> {
        let doc = std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/bus.md"))?;
        let cfg = toml::Value::try_from(Config::default())?;
        let settings: Vec<(String, String)> = cfg.as_table().into_iter().flatten().map(|(k, v)| (k.clone(), v.to_string())).collect();
        assert_eq!(settings.len(), 2, "{settings:?}");
        for (key, default) in settings {
            assert!(doc.contains(&format!("`{key}`")), "docs/bus.md doesn't name [bus] {key}");
            assert!(doc.contains(&format!("`{key} = {default}`")), "docs/bus.md doesn't give [bus] {key}'s default as `{key} = {default}`");
        }
        Ok(())
    }
}
