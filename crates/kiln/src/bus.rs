//! The bus kiln's events ride (docs/bus.md): Quiver, vendored at vendor/quiver, its node in the
//! server's process, behind kiln-seams' [`Bus`]. Quiver keeps each topic's messages in memory (the
//! ephemeral class, Focal's doc 33), each a Focal batch charged to one memory budget (`[bus]
//! memory`), and a reader behind what a topic keeps is refused `Behind`, never skipped silently.
//! The live feed of every session rides it (`crate::harness::live`). Classes other than ephemeral
//! are refused, typed, until their goals land (CP-T2.2 on).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

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

/// What the bus lab measured (goal 11, plan 01a1a87b2e56 step 3; docs/runtime-lab.md, "Measuring the bus's
/// bound"). The run was bus6, on fedora on 2026-11-05, of rt6's tree (`705fb481bfb`): kiln-lab's realistic profile
/// in production's layout, 8 rounds calm and 8 with hogs. Each round's server had `[bus] memory` at 1 GiB, which
/// none reached: no round refused a frame or sent a lag notice. The bus was sampled each second of every round
/// (`scripts/memory_lab/`). Its busiest second held 12,157,694 bytes for 11 topics.
mod lab {
    /// The most the bus held for each topic it held, framing included, at any second of the 16 rounds:
    /// 10,622,030 bytes for 8 topics, in a round with hogs.
    pub const TOPIC_SHARE: u64 = 1_327_754;
    /// The most one topic asked the bus to keep (`kiln_bus_topic_bytes_peak`): a feed's newest 2048 frames, their
    /// keys and payloads.
    pub const TOPIC_PEAK: u64 = 3_672_418;
}

/// The sessions production held (2026-11-05: 134 live feeds, kiln-lab's `Profile::many`). A topic is a watched
/// session's feed, so these are the topics production holds when every one of its sessions is watched at once.
const TOPICS_HELD: u64 = 134;

/// Room for one more of Quiver's largest messages with its framing: half the least a node takes
/// (`quiver::NodeConfig::MEMORY_MIN`, two of them and a segment), 1,083,392 bytes.
const LARGEST: u64 = (quiver::NodeConfig::MEMORY_MIN / 2) as u64;

const MIB: u64 = 1 << 20;

/// `n` rounded up to a whole MiB.
const fn up_to_mib(n: u64) -> u64 {
    n.div_ceil(MIB).saturating_mul(MIB)
}

/// The larger of `a` and `b`, in a constant.
const fn larger(a: u64, b: u64) -> u64 {
    if a > b { a } else { b }
}

/// The default `[bus] memory`: the lab's most for each topic ([`lab::TOPIC_SHARE`]), times as many topics as
/// production's sessions ([`TOPICS_HELD`]), with room for one more of Quiver's largest messages ([`LARGEST`]),
/// never below the least a node takes, and rounded up to a MiB: 179,306,496 bytes (171 MiB). That's the most for
/// a server of production's size. A server holding fewer topics holds less, because each topic is held to
/// `topic_bytes` ([`QuiverBus::bound`]). The lab's 11 topics took 12.2 MB at the most.
pub const MEMORY: u64 = up_to_mib(larger(lab::TOPIC_SHARE.saturating_mul(TOPICS_HELD).saturating_add(LARGEST), quiver::NodeConfig::MEMORY_MIN as u64));
/// The default `[bus] topic_bytes`: the largest topic the lab measured ([`lab::TOPIC_PEAK`]), with room for one
/// more of the largest message a topic takes (a payload at `quiver_api::PAYLOAD_MAX`; a feed's frames have no
/// key). It is never below that message, since Quiver refuses a message larger than `topic_bytes`, and is rounded
/// up to a MiB: 5 MiB. A topic past it keeps fewer than its 2048 frames, so its viewers fall behind sooner.
pub const TOPIC_BYTES: u64 = up_to_mib(larger(lab::TOPIC_PEAK.saturating_add(quiver_api::PAYLOAD_MAX as u64), quiver_api::PAYLOAD_MAX as u64));

/// The most Quiver charges a message past its key and payload, 86 bytes:
/// - its Focal batch's header: 56 bytes (`focal_bus::batch::HEADER`);
/// - its record's head there: its length's and delta's LEB128s, 20 bytes at the most (`RECORD_HEAD_MAX`);
/// - Quiver's own record head: the key's length as a LEB128, 10 bytes at the most.
///
/// `g11_s3_a_message_costs_the_bus_its_bytes_and_its_framing_at_most` measures it against the node.
const FRAMING: u64 = 56 + 20 + 10;

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
    /// The most messages any topic made on it retains: what its topics' framing is reckoned for ([`QuiverBus::bound`]).
    retain_most: Arc<AtomicU64>,
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
        Ok(Self { node, topic_bytes: cfg.topic_bytes, retain_most: Arc::new(AtomicU64::new(0)) })
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
    }
}

impl Bus for QuiverBus {
    fn create<'a>(&'a self, name: &'a str, topic: Topic) -> BoxFuture<'a, Result<(), Refusal>> {
        let retain_messages = u64::try_from(topic.retain).unwrap_or(u64::MAX);
        let quiver_topic = quiver_api::Topic { class: class(topic.class), retain_messages, retain_bytes: self.topic_bytes };
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

    /// Goal 11, plan 01a1a87b2e56 step 3: the defaults are the bus lab's, the numbers docs/bus.md and
    /// docs/runtime-lab.md give.
    /// - A server takes them as they are.
    /// - With the lab's 11 topics, each a feed of 2048 frames, the bound covers what the lab's busiest second held.
    /// - With production's sessions, every one watched, the limit binds.
    #[test]
    fn g11_s3_the_bus_defaults_are_derived_from_the_bus_lab() -> anyhow::Result<()> {
        anyhow::ensure!((MEMORY, TOPIC_BYTES) == (179_306_496, 5 << 20), "the defaults docs/bus.md gives: {MEMORY}, {TOPIC_BYTES}");
        let bus = quiver(&SimClock::new())?;
        anyhow::ensure!(u64::try_from(bus.stats().memory_limit)? == MEMORY, "a server's bus takes the default");
        futures::executor::block_on(bus.create("feed", Topic { class: Class::Ephemeral, retain: crate::harness::live::RETAIN }))?;
        let limit = usize::try_from(MEMORY)?;
        let at = |topics: usize| bus.bound(&quiver::Stats { topics, memory_limit: limit, ..quiver::Stats::default() });
        let busiest = 12_157_694;
        anyhow::ensure!(at(11) >= busiest, "11 topics are bound at {}, under the {busiest} bytes the lab's 11 held", at(11));
        anyhow::ensure!(at(usize::try_from(TOPICS_HELD)?) == MEMORY, "production's every session watched is bound by the limit");
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 3: a message costs the bus's memory its key's and payload's bytes and at
    /// most [`FRAMING`] more, whatever its size (each LEB128 of its framing from one byte to three) and whether
    /// or not it has a key. That's what [`QuiverBus::bound`] reckons each message kept at.
    #[test]
    fn g11_s3_a_message_costs_the_bus_its_bytes_and_its_framing_at_most() -> anyhow::Result<()> {
        // Room for every message here, so none is let go while it's measured.
        let bus = QuiverBus::open(&Config { memory: 256 << 20, topic_bytes: 64 << 20 }, Arc::new(SimClock::new()))?;
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
        let cfg = Config { memory: 64 << 20, topic_bytes: 64 << 10 };
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
        let small = QuiverBus::open(&Config { memory: u64::try_from(quiver::NodeConfig::MEMORY_MIN)?, topic_bytes: cfg.topic_bytes }, Arc::new(SimClock::new()))?;
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
        assert_eq!(settings.len(), 2, "{settings:?}");
        for (key, default) in settings {
            assert!(doc.contains(&format!("`{key}`")), "docs/bus.md doesn't name [bus] {key}");
            assert!(doc.contains(&format!("`{key} = {default}`")), "docs/bus.md doesn't give [bus] {key}'s default as `{key} = {default}`");
        }
        Ok(())
    }
}
