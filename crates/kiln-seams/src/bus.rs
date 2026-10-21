//! `Bus`: topics of messages. A topic's messages have offsets in the order they were published;
//! any number of readers each read from any offset still kept, and wait for the next. A topic
//! keeps its newest messages up to its retention: a reader behind them is told where they start
//! (`Behind`), never skipped silently. Each topic states its durability class (Focal's doc 33:
//! ephemeral, replicated, persisted, windowed). Quiver is the real one (CP-T2.1); [`SimBus`] keeps
//! topics in memory (ephemeral, whatever class is asked).

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use futures::future::BoxFuture;
use parking_lot::Mutex;

use crate::clock::{Clock, SimClock};
use crate::conformance::{Case, Failure, ensure, fine, refused, same};

/// A message's place in its topic: 0 for the first ever published.
pub type Offset = u64;

/// How a topic's messages outlive failures (Focal's doc 33).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// In memory, lost with its node.
    Ephemeral,
    /// In the memory of a quorum, on independent fates.
    Replicated,
    /// Synced to disk by a quorum.
    Persisted,
    /// A bounded window, for high-rate telemetry.
    Windowed,
}

/// A topic as created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Topic {
    pub class: Class,
    /// Messages kept: the newest, past it the oldest go.
    pub retain: usize,
}

/// A message as read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub offset: Offset,
    pub key: Option<Vec<u8>>,
    pub payload: Vec<u8>,
    /// When it was published (its bus's wall clock, ms since the epoch).
    pub at_ms: i64,
}

/// Messages read from an offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    pub messages: Vec<Message>,
    /// Where the next read goes on from.
    pub next: Offset,
}

/// Why a request was refused.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("no topic {0:?}")]
    NoTopic(String),
    #[error("topic {0:?} exists as something else")]
    Exists(String),
    #[error("{what} isn't valid: {why}")]
    Invalid { what: &'static str, why: String },
    #[error("{what} is {size}, over its limit of {limit}")]
    TooLarge { what: &'static str, size: usize, limit: usize },
    #[error("the bus holds its most topics ({limit})")]
    Full { limit: usize },
    /// The messages asked for are gone: those kept start at `first`.
    #[error("offset {asked} is gone: the topic's messages start at {first}")]
    Behind { asked: Offset, first: Offset },
    #[error("the bus is unavailable: {0}")]
    Unavailable(String),
}

/// Topics of messages.
pub trait Bus: Send + Sync + 'static {
    /// Creates `name` (again with the same settings: nothing changes).
    fn create<'a>(&'a self, name: &'a str, topic: Topic) -> BoxFuture<'a, Result<(), Refusal>>;
    /// Publishes a message, returning its offset.
    fn publish<'a>(&'a self, name: &'a str, key: Option<Vec<u8>>, payload: Vec<u8>) -> BoxFuture<'a, Result<Offset, Refusal>>;
    /// Messages from offset `from`, oldest first, at most `limit`.
    fn read<'a>(&'a self, name: &'a str, from: Offset, limit: usize) -> BoxFuture<'a, Result<Batch, Refusal>>;
    /// Completes once a message at `from` or later is there (at once when one is).
    fn wait<'a>(&'a self, name: &'a str, from: Offset) -> BoxFuture<'a, Result<(), Refusal>>;
    /// The first offset kept and the next to be given.
    fn bounds<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(Offset, Offset), Refusal>>;
}

/// Longest topic name.
pub const NAME_MAX: usize = 128;
/// Largest payload.
pub const PAYLOAD_MAX: usize = 1 << 20;
/// Longest key.
pub const KEY_MAX: usize = 1024;
/// Most messages a read returns.
pub const READ_MAX: usize = 1000;
/// Most messages a topic may retain.
pub const RETAIN_MAX: usize = 1_000_000;

fn check_name(name: &str) -> Result<(), Refusal> {
    let ok = !name.is_empty() && name.len() <= NAME_MAX && name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/'));
    if ok { Ok(()) } else { Err(Refusal::Invalid { what: "a topic name", why: format!("{name:?}: 1 to {NAME_MAX} of A-Z, a-z, 0-9, _ - . /") }) }
}

// ---------------------------------------------------------------------------------- simulator

/// Most topics a [`SimBus`] holds.
pub const SIM_TOPICS: usize = 10_000;

/// Topics in memory, stamped by a clock. Clones share them.
#[derive(Clone)]
pub struct SimBus {
    inner: Arc<SimInner>,
    clock: Arc<dyn Clock>,
}

#[derive(Default)]
struct SimInner {
    topics: Mutex<BTreeMap<String, SimTopic>>,
    published: event_listener::Event,
}

struct SimTopic {
    topic: Topic,
    first: Offset,
    messages: VecDeque<Message>,
}

impl SimTopic {
    fn next(&self) -> Offset {
        self.first.saturating_add(u64::try_from(self.messages.len()).unwrap_or(u64::MAX))
    }
}

impl Default for SimBus {
    fn default() -> Self {
        Self::new(Arc::new(SimClock::new()))
    }
}

impl SimBus {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self { inner: Arc::default(), clock }
    }
}

impl Bus for SimBus {
    fn create<'a>(&'a self, name: &'a str, topic: Topic) -> BoxFuture<'a, Result<(), Refusal>> {
        let out = (|| {
            check_name(name)?;
            if topic.retain == 0 || topic.retain > RETAIN_MAX {
                return Err(Refusal::Invalid { what: "a topic's retention", why: format!("{} (1 to {RETAIN_MAX} messages)", topic.retain) });
            }
            let mut topics = self.inner.topics.lock();
            if let Some(t) = topics.get(name) {
                return if t.topic == topic { Ok(()) } else { Err(Refusal::Exists(name.to_owned())) };
            }
            if topics.len() >= SIM_TOPICS {
                return Err(Refusal::Full { limit: SIM_TOPICS });
            }
            topics.insert(name.to_owned(), SimTopic { topic, first: 0, messages: VecDeque::new() });
            Ok(())
        })();
        Box::pin(async move { out })
    }

    fn publish<'a>(&'a self, name: &'a str, key: Option<Vec<u8>>, payload: Vec<u8>) -> BoxFuture<'a, Result<Offset, Refusal>> {
        let at_ms = self.clock.unix_ms();
        let out = (|| {
            if payload.len() > PAYLOAD_MAX {
                return Err(Refusal::TooLarge { what: "a payload", size: payload.len(), limit: PAYLOAD_MAX });
            }
            if let Some(k) = &key
                && k.len() > KEY_MAX
            {
                return Err(Refusal::TooLarge { what: "a key", size: k.len(), limit: KEY_MAX });
            }
            let mut topics = self.inner.topics.lock();
            let t = topics.get_mut(name).ok_or_else(|| Refusal::NoTopic(name.to_owned()))?;
            let offset = t.next();
            t.messages.push_back(Message { offset, key, payload, at_ms });
            while t.messages.len() > t.topic.retain {
                if t.messages.pop_front().is_some() {
                    t.first = t.first.saturating_add(1);
                }
            }
            Ok(offset)
        })();
        if out.is_ok() {
            self.inner.published.notify(usize::MAX);
        }
        Box::pin(async move { out })
    }

    fn read<'a>(&'a self, name: &'a str, from: Offset, limit: usize) -> BoxFuture<'a, Result<Batch, Refusal>> {
        let out = (|| {
            if limit == 0 {
                return Err(Refusal::Invalid { what: "a read's limit", why: "it's 0".into() });
            }
            let topics = self.inner.topics.lock();
            let t = topics.get(name).ok_or_else(|| Refusal::NoTopic(name.to_owned()))?;
            if from < t.first {
                return Err(Refusal::Behind { asked: from, first: t.first });
            }
            let skip = usize::try_from(from.saturating_sub(t.first)).unwrap_or(usize::MAX);
            let messages: Vec<Message> = t.messages.iter().skip(skip).take(limit.min(READ_MAX)).cloned().collect();
            let next = messages.last().map_or_else(|| from.max(t.first).min(t.next()), |m| m.offset.saturating_add(1));
            Ok(Batch { messages, next })
        })();
        Box::pin(async move { out })
    }

    fn wait<'a>(&'a self, name: &'a str, from: Offset) -> BoxFuture<'a, Result<(), Refusal>> {
        Box::pin(async move {
            loop {
                let listener = self.inner.published.listen();
                {
                    let topics = self.inner.topics.lock();
                    let t = topics.get(name).ok_or_else(|| Refusal::NoTopic(name.to_owned()))?;
                    if t.next() > from {
                        return Ok(());
                    }
                }
                listener.await;
            }
        })
    }

    fn bounds<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(Offset, Offset), Refusal>> {
        let out = self.inner.topics.lock().get(name).map(|t| (t.first, t.next())).ok_or_else(|| Refusal::NoTopic(name.to_owned()));
        Box::pin(async move { out })
    }
}

// -------------------------------------------------------------------------------- conformance

/// What a `Bus` conformance suite runs against: an empty bus per case.
pub trait BusFixture: Send + Sync {
    fn bus(&self) -> &dyn Bus;
}

/// The fixture for a [`SimBus`].
pub struct SimBusFixture(pub SimBus);

impl BusFixture for SimBusFixture {
    fn bus(&self) -> &dyn Bus {
        &self.0
    }
}

const T: &str = "conformance";

fn topic(retain: usize) -> Topic {
    Topic { class: Class::Ephemeral, retain }
}

fn payloads(b: &Batch) -> Vec<String> {
    b.messages.iter().map(|m| String::from_utf8_lossy(&m.payload).into_owned()).collect()
}

async fn publish_all(bus: &dyn Bus, name: &str, ps: &[&str]) -> Result<Vec<Offset>, Failure> {
    let mut out = Vec::new();
    for p in ps {
        out.push(fine("publish", bus.publish(name, None, p.as_bytes().to_vec()).await)?);
    }
    Ok(out)
}

fn c1<F: BusFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.bus();
        fine("create", b.create(T, topic(100)).await)?;
        same("the offsets given", publish_all(b, T, &["a", "b", "c"]).await?, vec![0, 1, 2])?;
        same("the bounds", fine("bounds", b.bounds(T).await)?, (0, 3))
    })
}

fn c2<F: BusFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.bus();
        fine("create", b.create(T, topic(100)).await)?;
        fine("publish", b.publish(T, Some(b"k1".to_vec()), b"first".to_vec()).await)?;
        fine("publish", b.publish(T, None, b"second".to_vec()).await)?;
        let all = fine("read", b.read(T, 0, 10).await)?;
        same("what was read", payloads(&all), vec!["first".to_owned(), "second".to_owned()])?;
        same("the first message's key", all.messages.first().and_then(|m| m.key.clone()), Some(b"k1".to_vec()))?;
        same("where reading goes on", all.next, 2)?;
        let tail = fine("read from 1", b.read(T, 1, 10).await)?;
        same("a read from an offset", payloads(&tail), vec!["second".to_owned()])
    })
}

fn c3<F: BusFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.bus();
        fine("create", b.create(T, topic(100)).await)?;
        publish_all(b, T, &["x", "y"]).await?;
        let one = fine("a first reader", b.read(T, 0, 10).await)?;
        let two = fine("a second reader", b.read(T, 0, 10).await)?;
        same("what two readers read", payloads(&one), payloads(&two))?;
        same("how many", one.messages.len(), 2)
    })
}

fn c4<F: BusFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.bus();
        fine("create", b.create(T, topic(2)).await)?;
        publish_all(b, T, &["a", "b", "c", "d"]).await?;
        same("the bounds past retention", fine("bounds", b.bounds(T).await)?, (2, 4))?;
        let e = refused("a read from before what's kept", b.read(T, 0, 10).await)?;
        same("the refusal", e, Refusal::Behind { asked: 0, first: 2 })?;
        same("what's kept", payloads(&fine("read", b.read(T, 2, 10).await)?), vec!["c".to_owned(), "d".to_owned()])
    })
}

fn c5<F: BusFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.bus();
        fine("create", b.create(T, topic(100)).await)?;
        publish_all(b, T, &["a"]).await?;
        ensure(futures::FutureExt::now_or_never(b.wait(T, 0)).is_some_and(|r| r.is_ok()), || "a wait for a message already there didn't complete at once".into())?;
        let mut waiting = b.wait(T, 1);
        ensure(futures::FutureExt::now_or_never(&mut waiting).is_none(), || "a wait for a message not yet published completed".into())?;
        publish_all(b, T, &["b"]).await?;
        same("the wait, once it's published", waiting.await, Ok(()))
    })
}

fn c6<F: BusFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.bus();
        same("a publish to no topic", b.publish("conformance-none", None, b"x".to_vec()).await, Err(Refusal::NoTopic("conformance-none".into())))?;
        same("a read of no topic", b.read("conformance-none", 0, 1).await, Err(Refusal::NoTopic("conformance-none".into())))
    })
}

fn c7<F: BusFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.bus();
        fine("create", b.create(T, topic(10)).await)?;
        fine("the same create again", b.create(T, topic(10)).await)?;
        same("a create with other settings", b.create(T, topic(11)).await, Err(Refusal::Exists(T.into())))?;
        ensure(matches!(b.create("Not a name!", topic(10)).await, Err(Refusal::Invalid { .. })), || "a bad topic name wasn't refused Invalid".into())?;
        ensure(matches!(b.create("conformance-zero", topic(0)).await, Err(Refusal::Invalid { .. })), || "a retention of 0 wasn't refused Invalid".into())
    })
}

fn c8<F: BusFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.bus();
        fine("create", b.create(T, topic(10)).await)?;
        let e = refused("a payload over the limit", b.publish(T, None, vec![0; PAYLOAD_MAX.saturating_add(1)]).await)?;
        ensure(matches!(e, Refusal::TooLarge { .. }), || format!("refused as {e:?}"))?;
        same("the bounds after it", fine("bounds", b.bounds(T).await)?, (0, 0))
    })
}

fn c9<F: BusFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.bus();
        fine("create", b.create(T, topic(100)).await)?;
        publish_all(b, T, &["a", "b", "c", "d", "e"]).await?;
        let first = fine("read", b.read(T, 0, 2).await)?;
        same("a limited read", (payloads(&first), first.next), (vec!["a".to_owned(), "b".to_owned()], 2))?;
        let rest = fine("read on", b.read(T, first.next, 10).await)?;
        same("reading on", (payloads(&rest), rest.next), (vec!["c".to_owned(), "d".to_owned(), "e".to_owned()], 5))?;
        let none = fine("read at the end", b.read(T, rest.next, 10).await)?;
        same("a read at the end", (none.messages.len(), none.next), (0, 5))?;
        ensure(matches!(b.read(T, 0, 0).await, Err(Refusal::Invalid { .. })), || "a read of 0 wasn't refused Invalid".into())
    })
}

fn c10<F: BusFixture>(f: &F) -> BoxFuture<'_, Result<(), Failure>> {
    Box::pin(async move {
        let b = f.bus();
        fine("create", b.create("conformance-one", topic(10)).await)?;
        fine("create", b.create("conformance-two", topic(10)).await)?;
        publish_all(b, "conformance-one", &["only in one"]).await?;
        same("the other topic", fine("read", b.read("conformance-two", 0, 10).await)?.messages.len(), 0)?;
        same("the other topic's offsets", publish_all(b, "conformance-two", &["first there"]).await?, vec![0])
    })
}

/// The `Bus` contract, clause by clause (docs/seams.md, "Bus").
pub fn cases<F: BusFixture>() -> Vec<Case<F>> {
    vec![
        Case { id: "BUS-1", says: "a topic's messages get offsets 0, 1, 2… in the order they were published", check: c1::<F> },
        Case { id: "BUS-2", says: "a read from an offset gives the messages from it in publish order, keys and payloads intact, and where to go on", check: c2::<F> },
        Case { id: "BUS-3", says: "every reader reads every message kept: readers don't take them from each other", check: c3::<F> },
        Case { id: "BUS-4", says: "a topic keeps its newest messages up to its retention; a read from before them is refused Behind, naming where they start", check: c4::<F> },
        Case { id: "BUS-5", says: "a wait completes once a message at its offset or later is there, at once when one is", check: c5::<F> },
        Case { id: "BUS-6", says: "a publish to or read of a topic that doesn't exist is refused NoTopic", check: c6::<F> },
        Case { id: "BUS-7", says: "creating a topic again with the same settings changes nothing; with others it's refused Exists; bad names and a retention of 0 are refused", check: c7::<F> },
        Case { id: "BUS-8", says: "a payload over the limit is refused TooLarge and publishes nothing", check: c8::<F> },
        Case { id: "BUS-9", says: "a read gives at most its limit and says where to go on; at the end it's empty; a limit of 0 is refused", check: c9::<F> },
        Case { id: "BUS-10", says: "topics are apart: each has its own messages and offsets", check: c10::<F> },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::run;

    /// CP-M2, AC-M2.2: the simulator and its suite.
    #[test]
    fn the_simulated_bus_passes_the_bus_suite() -> Result<(), String> {
        futures::executor::block_on(run("Bus", &cases::<SimBusFixture>(), || async { SimBusFixture(SimBus::default()) })).into_result().map(|_| ())
    }

    /// CP-M2, AC-M2.2: the simulator, on the simulated clock.
    #[test]
    fn messages_are_stamped_by_the_bus_clock() {
        let clock = SimClock::new();
        let bus = SimBus::new(Arc::new(clock.clone()));
        assert!(futures::executor::block_on(bus.create(T, topic(10))).is_ok());
        clock.advance(std::time::Duration::from_millis(1500));
        assert!(futures::executor::block_on(bus.publish(T, None, b"x".to_vec())).is_ok());
        let read = futures::executor::block_on(bus.read(T, 0, 1));
        assert_eq!(read.ok().and_then(|b| b.messages.first().map(|m| m.at_ms)), Some(crate::clock::SIM_EPOCH_MS + 1500));
    }
}
