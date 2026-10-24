//! One Quiver node, in this process: its topics by name, each a log of its class, every byte its
//! messages take charged to one memory budget (Focal's `focal_memory`), and as many topics and
//! subscriptions as it states. It serves [`quiver_api::Topics`].
//!
//! It serves the ephemeral class (`crate::ephemeral`). The replicated and persisted classes come
//! with Focal's replicated part (kiln's CP-T2.2), the windowed with CP-T2.4: until then a topic of
//! those classes is refused [`Refusal::Unsupported`], never kept as another class.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use focal_memory::MemoryBudget;
use futures::future::BoxFuture;
use quiver_api::{
    Batch, Class, Offset, Refusal, Start, Subscription, Topic, Topics, check_name, check_topic,
};

use crate::ephemeral::Log;

/// What a node holds at most.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NodeConfig {
    /// The bytes its messages may take, framing included, until their readers let them go.
    pub memory: usize,
    /// Topics at once.
    pub topics: usize,
    /// Subscriptions at once.
    pub subscriptions: usize,
}

impl NodeConfig {
    /// The least memory a node takes: a segment's bytes and two of the largest messages (a payload
    /// and a key at their limits, and their batch's framing, under 1 KiB), so a topic keeping its
    /// largest message, held by a reader, can still take another.
    pub const MEMORY_MIN: usize = quiver_api::PAYLOAD_MAX
        .saturating_add(quiver_api::KEY_MAX)
        .saturating_add(1024)
        .saturating_mul(2)
        .saturating_add(crate::ephemeral::SEGMENT_BYTES);
}

/// A clock for stamping messages: µs since the Unix epoch.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The system's wall clock, µs since the Unix epoch (0 before it).
#[must_use]
pub fn wall_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
    })
}

/// A node's figures, as they stand.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Stats {
    pub topics: usize,
    pub subscriptions: usize,
    /// Messages published since the node started, on topics it holds now.
    pub published: u64,
    /// Lag notices subscriptions took, on topics it holds now.
    pub lags: u64,
    /// Bytes its messages take now, framing included.
    pub memory_used: usize,
    pub memory_limit: usize,
}

/// One node. Clones share it.
#[derive(Clone)]
pub struct Node {
    inner: Arc<Inner>,
}

struct Inner {
    config: NodeConfig,
    budget: MemoryBudget,
    topics: RwLock<HashMap<String, Arc<Log>>>,
    /// The Focal stream the next topic takes.
    streams: AtomicU64,
    /// Subscriptions live.
    live: Arc<AtomicUsize>,
    clock: Clock,
}

impl Node {
    /// A node of `config`, stamping messages by the system's wall clock.
    pub fn new(config: NodeConfig) -> Result<Self, Refusal> {
        Self::with_clock(config, wall_clock())
    }

    /// A node of `config`, stamping messages by `clock`.
    pub fn with_clock(config: NodeConfig, clock: Clock) -> Result<Self, Refusal> {
        if config.memory < NodeConfig::MEMORY_MIN {
            return Err(Refusal::Invalid {
                what: "a node's memory",
                why: format!(
                    "{} bytes, under the least a node takes ({})",
                    config.memory,
                    NodeConfig::MEMORY_MIN
                ),
            });
        }
        if config.topics == 0 || config.subscriptions == 0 {
            return Err(Refusal::Invalid {
                what: "a node's bounds",
                why: "it holds at least one topic and one subscription".to_owned(),
            });
        }
        let budget = MemoryBudget::new(config.memory, 0).map_err(|e| Refusal::Invalid {
            what: "a node's memory",
            why: e.to_string(),
        })?;
        Ok(Self {
            inner: Arc::new(Inner {
                config,
                budget,
                topics: RwLock::new(HashMap::new()),
                streams: AtomicU64::new(1),
                live: Arc::new(AtomicUsize::new(0)),
                clock,
            }),
        })
    }

    /// What it holds at most.
    #[must_use]
    pub fn config(&self) -> NodeConfig {
        self.inner.config
    }

    /// Its figures now.
    #[must_use]
    pub fn stats(&self) -> Stats {
        let topics = self
            .inner
            .topics
            .read()
            .unwrap_or_else(PoisonError::into_inner);
        let (published, lags) = topics.values().fold((0_u64, 0_u64), |(p, l), log| {
            (
                p.saturating_add(log.published.load(Ordering::Relaxed)),
                l.saturating_add(log.lags.load(Ordering::Relaxed)),
            )
        });
        let budget = self.inner.budget.stats();
        Stats {
            topics: topics.len(),
            subscriptions: self.inner.live.load(Ordering::Relaxed),
            published,
            lags,
            memory_used: budget.used,
            memory_limit: budget.limit,
        }
    }

    fn log(&self, name: &str) -> Result<Arc<Log>, Refusal> {
        self.inner
            .topics
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(name)
            .cloned()
            .ok_or_else(|| Refusal::NoTopic(name.to_owned()))
    }

    fn create_now(&self, name: &str, topic: Topic) -> Result<(), Refusal> {
        check_name(name)?;
        check_topic(&topic)?;
        if topic.class != Class::Ephemeral {
            return Err(Refusal::Unsupported(topic.class));
        }
        let mut topics = self
            .inner
            .topics
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(log) = topics.get(name) {
            return if log.topic == topic {
                Ok(())
            } else {
                Err(Refusal::Exists(name.to_owned()))
            };
        }
        if topics.len() >= self.inner.config.topics {
            return Err(Refusal::Full {
                what: "the node's topics",
                limit: u64::try_from(self.inner.config.topics).unwrap_or(u64::MAX),
            });
        }
        let stream = self.inner.streams.fetch_add(1, Ordering::Relaxed);
        topics.insert(
            name.to_owned(),
            Arc::new(Log::new(topic, stream, self.inner.budget.clone())),
        );
        Ok(())
    }

    fn delete_now(&self, name: &str) -> Result<(), Refusal> {
        let log = self
            .inner
            .topics
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(name)
            .ok_or_else(|| Refusal::NoTopic(name.to_owned()))?;
        log.close();
        Ok(())
    }

    fn subscribe_now(&self, name: &str, start: Start) -> Result<Box<dyn Subscription>, Refusal> {
        let log = self.log(name)?;
        let limit = self.inner.config.subscriptions;
        // A place is taken before the subscription is made, given back at once if it's past the
        // bound, and by the subscription as it's dropped: each place taken is given back once.
        let live = Arc::clone(&self.inner.live);
        if live.fetch_add(1, Ordering::AcqRel) >= limit {
            live.fetch_sub(1, Ordering::AcqRel);
            return Err(Refusal::Full {
                what: "the node's subscriptions",
                limit: u64::try_from(limit).unwrap_or(u64::MAX),
            });
        }
        match log.subscribe(start, Arc::clone(&live)) {
            Ok(subscription) => Ok(Box::new(subscription)),
            Err(refusal) => {
                live.fetch_sub(1, Ordering::AcqRel);
                Err(refusal)
            }
        }
    }
}

impl Topics for Node {
    fn create<'a>(&'a self, name: &'a str, topic: Topic) -> BoxFuture<'a, Result<(), Refusal>> {
        let out = self.create_now(name, topic);
        Box::pin(async move { out })
    }

    fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), Refusal>> {
        let out = self.delete_now(name);
        Box::pin(async move { out })
    }

    fn publish<'a>(
        &'a self,
        name: &'a str,
        key: Option<&'a [u8]>,
        payload: &'a [u8],
    ) -> BoxFuture<'a, Result<Offset, Refusal>> {
        let out = self
            .log(name)
            .and_then(|log| log.publish(name, key, payload, (self.inner.clock)()));
        Box::pin(async move { out })
    }

    fn read<'a>(
        &'a self,
        name: &'a str,
        from: Offset,
        limit: usize,
    ) -> BoxFuture<'a, Result<Batch, Refusal>> {
        let out = self.log(name).and_then(|log| log.read(from, limit));
        Box::pin(async move { out })
    }

    fn wait<'a>(&'a self, name: &'a str, from: Offset) -> BoxFuture<'a, Result<(), Refusal>> {
        let log = self.log(name);
        Box::pin(async move { log?.wait(name, from).await })
    }

    fn bounds<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(Offset, Offset), Refusal>> {
        let out = self.log(name).map(|log| log.bounds());
        Box::pin(async move { out })
    }

    fn subscribe<'a>(
        &'a self,
        name: &'a str,
        start: Start,
    ) -> BoxFuture<'a, Result<Box<dyn Subscription>, Refusal>> {
        let out = self.subscribe_now(name, start);
        Box::pin(async move { out })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicU64;

    use futures::FutureExt as _;
    use quiver_api::{Next, PAYLOAD_MAX};

    use super::*;

    fn now<T>(f: BoxFuture<'_, T>) -> T {
        f.now_or_never().expect("the node answers at once")
    }

    fn node(memory: usize, topics: usize, subscriptions: usize) -> Node {
        Node::new(NodeConfig {
            memory,
            topics,
            subscriptions,
        })
        .unwrap()
    }

    fn ephemeral(retain_messages: u64, retain_bytes: u64) -> Topic {
        Topic {
            class: Class::Ephemeral,
            retain_messages,
            retain_bytes,
        }
    }

    /// AC-T2.1.1: do a publish with a key, expect it kept as a Focal batch of one record for its
    /// topic's stream, the record Quiver's framing: the key's length plus one, the key, the payload.
    #[test]
    fn each_message_is_kept_as_a_focal_batch_of_one_record() {
        let n = node(64 << 20, 10, 10);
        now(n.create("first", ephemeral(10, 1 << 20))).unwrap();
        now(n.create("second", ephemeral(10, 1 << 20))).unwrap();
        now(n.publish("second", Some(b"k"), b"value")).unwrap();
        now(n.publish("second", None, b"bare")).unwrap();
        let log = n.log("second").unwrap();
        assert_eq!(
            crate::ephemeral::focal_batch_of(&log, 0),
            Some((2, 1, b"\x02kvalue".to_vec()))
        );
        assert_eq!(
            crate::ephemeral::focal_batch_of(&log, 1),
            Some((2, 1, b"\x00bare".to_vec()))
        );
    }

    /// The node's memory bounds what it keeps: do publish 200 messages of 256 KiB into a topic
    /// whose retention is past the memory, expect every publish taken, the oldest let go, and the
    /// bytes held never past the limit.
    #[test]
    fn the_nodes_memory_bounds_what_it_keeps() {
        let n = node(NodeConfig::MEMORY_MIN, 10, 10);
        now(n.create("big", ephemeral(1_000_000, 1 << 32))).unwrap();
        let payload = vec![7; 256 << 10];
        for i in 0..200_u64 {
            assert_eq!(now(n.publish("big", None, &payload)), Ok(i));
            let stats = n.stats();
            assert!(stats.memory_used <= stats.memory_limit, "{stats:?}");
        }
        let (first, end) = now(n.bounds("big")).unwrap();
        assert_eq!(end, 200);
        assert!(first > 190, "the oldest weren't let go: {first}");
    }

    /// A reader holding what the budget needs: do hold a subscription on a topic's largest
    /// messages while another topic fills the memory, expect a typed refusal, never a silent drop;
    /// then let the reader go and expect the publish taken.
    #[test]
    fn a_full_memory_refuses_a_publish_typed_and_takes_it_once_readers_let_go() {
        let n = node(NodeConfig::MEMORY_MIN, 10, 10);
        now(n.create("held", ephemeral(10, 1 << 32))).unwrap();
        now(n.create("other", ephemeral(10, 1 << 32))).unwrap();
        let largest = vec![1; PAYLOAD_MAX];
        now(n.publish("held", None, &largest)).unwrap();
        let mut reader = now(n.subscribe("held", Start::Oldest)).unwrap();
        now(n.publish("held", None, &largest)).unwrap();
        // "held" keeps its two largest messages; "other" can't keep one more beside them.
        let refused = now(n.publish("other", None, &largest));
        assert!(
            matches!(
                refused,
                Err(Refusal::Full {
                    what: "the node's memory",
                    ..
                })
            ),
            "{refused:?}"
        );
        assert_eq!(
            now(n.bounds("other")),
            Ok((0, 0)),
            "a refused publish changed the topic"
        );
        assert!(matches!(reader.take(), Some(Next::Message(m)) if m.offset == 0));
        now(n.delete("held")).unwrap();
        assert!(
            n.stats().memory_used > 0,
            "the reader still holds its segment"
        );
        drop(reader);
        assert_eq!(now(n.publish("other", None, &largest)), Ok(0));
    }

    /// Every byte goes back: do publish, subscribe, delete, then let the subscription go, expect
    /// the node's memory at 0.
    #[test]
    fn a_deleted_topic_gives_its_memory_back_once_its_readers_let_go() {
        let n = node(64 << 20, 10, 10);
        now(n.create("t", ephemeral(1000, 1 << 20))).unwrap();
        for i in 0..300_u32 {
            now(n.publish("t", None, &i.to_le_bytes())).unwrap();
        }
        let mut sub = now(n.subscribe("t", Start::Oldest)).unwrap();
        assert!(matches!(sub.take(), Some(Next::Message(_))));
        assert!(n.stats().memory_used > 0);
        now(n.delete("t")).unwrap();
        assert_eq!(sub.take(), Some(Next::Ended));
        drop(sub);
        assert_eq!(n.stats().memory_used, 0);
    }

    /// Bounds hold and places come back: do subscribe past the node's subscriptions and create past
    /// its topics, expect typed refusals; let one go, expect its place taken again.
    #[test]
    fn subscriptions_and_topics_are_bounded_and_their_places_come_back() {
        let n = node(64 << 20, 1, 2);
        now(n.create("a", ephemeral(10, 1 << 20))).unwrap();
        assert!(matches!(
            now(n.create("b", ephemeral(10, 1 << 20))),
            Err(Refusal::Full {
                what: "the node's topics",
                limit: 1
            })
        ));
        let one = now(n.subscribe("a", Start::Oldest)).unwrap();
        let _two = now(n.subscribe("a", Start::Oldest)).unwrap();
        assert!(matches!(
            now(n.subscribe("a", Start::Oldest)).map(|_| ()),
            Err(Refusal::Full {
                what: "the node's subscriptions",
                limit: 2
            })
        ));
        drop(one);
        assert_eq!(n.stats().subscriptions, 1);
        assert!(now(n.subscribe("a", Start::Oldest)).is_ok());
        now(n.delete("a")).unwrap();
        assert!(now(n.create("b", ephemeral(10, 1 << 20))).is_ok());
    }

    /// Stamps rise with offsets whatever the clock does: do publish under a clock that steps back,
    /// expect each stamp at least the one before.
    #[test]
    fn stamps_rise_with_offsets_when_the_clock_steps_back() {
        let ticks = Arc::new(AtomicU64::new(5_000_000));
        let clock: Clock = {
            let ticks = Arc::clone(&ticks);
            Arc::new(move || ticks.load(Ordering::Relaxed))
        };
        let n = Node::with_clock(
            NodeConfig {
                memory: 64 << 20,
                topics: 1,
                subscriptions: 1,
            },
            clock,
        )
        .unwrap();
        now(n.create("t", ephemeral(10, 1 << 20))).unwrap();
        now(n.publish("t", None, b"a")).unwrap();
        ticks.store(1_000_000, Ordering::Relaxed);
        now(n.publish("t", None, b"b")).unwrap();
        ticks.store(9_000_000, Ordering::Relaxed);
        now(n.publish("t", None, b"c")).unwrap();
        let stamps: Vec<u64> = now(n.read("t", 0, 10))
            .unwrap()
            .messages
            .iter()
            .map(|m| m.at_us)
            .collect();
        assert_eq!(stamps, [5_000_000, 5_000_000, 9_000_000]);
    }

    /// A node too small to keep its largest message is refused as it's made.
    #[test]
    fn a_node_too_small_for_its_largest_message_is_refused() {
        assert!(matches!(
            Node::new(NodeConfig {
                memory: NodeConfig::MEMORY_MIN - 1,
                topics: 1,
                subscriptions: 1
            }),
            Err(Refusal::Invalid { .. })
        ));
        assert!(matches!(
            Node::new(NodeConfig {
                memory: NodeConfig::MEMORY_MIN,
                topics: 0,
                subscriptions: 1
            }),
            Err(Refusal::Invalid { .. })
        ));
    }
}
