//! The simulator: topics in memory, deterministic, each message stamped by a clock the test moves.
//! It serves every class as ephemeral (whatever a topic states, its messages live in this
//! process's memory), so code built against [`Topics`] runs in a test without a node. It passes the
//! conformance suite, as Quiver's node does.

use std::collections::{BTreeMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use event_listener::{Event, EventListener};
use futures::future::BoxFuture;

use crate::{
    Batch, Message, MessageRef, Next, Offset, Refusal, Start, Subscription, Topic, Topics,
    check_limit, check_message, check_name, check_topic,
};

/// Most topics a [`SimTopics`] holds.
pub const SIM_TOPICS: usize = 10_000;
/// Where a simulator's clock starts: 2026-10-20T00:00:00Z, in µs since the Unix epoch.
pub const SIM_EPOCH_US: u64 = 1_792_454_400_000_000;

/// Topics in memory. Clones share them.
#[derive(Clone)]
pub struct SimTopics {
    inner: Arc<Inner>,
}

struct Inner {
    topics: Mutex<BTreeMap<String, Arc<SimTopic>>>,
    now_us: AtomicU64,
}

struct SimTopic {
    topic: Topic,
    log: Mutex<Log>,
    /// Notified at each publish and at the topic's deletion.
    changed: Event,
    deleted: AtomicBool,
}

#[derive(Default)]
struct Log {
    /// The first offset kept.
    first: Offset,
    messages: VecDeque<Message>,
    /// The bytes the kept messages count against the topic's retention.
    bytes: u64,
}

impl Log {
    /// The offset the next message takes.
    fn end(&self) -> Offset {
        self.first
            .saturating_add(u64::try_from(self.messages.len()).unwrap_or(u64::MAX))
    }
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Default for SimTopics {
    fn default() -> Self {
        Self::new()
    }
}

impl SimTopics {
    /// No topics, its clock at [`SIM_EPOCH_US`].
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                topics: Mutex::new(BTreeMap::new()),
                now_us: AtomicU64::new(SIM_EPOCH_US),
            }),
        }
    }

    /// Moves its clock on by `by`: the next message published is stamped that much later.
    pub fn advance(&self, by: Duration) {
        // µs since 1970 in a u64 wrap 584,000 years on: the clock's add can't reach it.
        self.inner.now_us.fetch_add(
            u64::try_from(by.as_micros()).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    fn topic(&self, name: &str) -> Result<Arc<SimTopic>, Refusal> {
        lock(&self.inner.topics)
            .get(name)
            .cloned()
            .ok_or_else(|| Refusal::NoTopic(name.to_owned()))
    }

    fn create_now(&self, name: &str, topic: Topic) -> Result<(), Refusal> {
        check_name(name)?;
        check_topic(&topic)?;
        let mut topics = lock(&self.inner.topics);
        if let Some(t) = topics.get(name) {
            return if t.topic == topic {
                Ok(())
            } else {
                Err(Refusal::Exists(name.to_owned()))
            };
        }
        if topics.len() >= SIM_TOPICS {
            return Err(Refusal::Full {
                what: "the simulator's topics",
                limit: u64::try_from(SIM_TOPICS).unwrap_or(u64::MAX),
            });
        }
        topics.insert(
            name.to_owned(),
            Arc::new(SimTopic {
                topic,
                log: Mutex::new(Log::default()),
                changed: Event::new(),
                deleted: AtomicBool::new(false),
            }),
        );
        Ok(())
    }

    fn publish_now(
        &self,
        name: &str,
        key: Option<&[u8]>,
        payload: &[u8],
    ) -> Result<Offset, Refusal> {
        let t = self.topic(name)?;
        let bytes = check_message(&t.topic, key, payload)?;
        let at_us = self.inner.now_us.load(Ordering::Relaxed);
        let offset = {
            let mut log = lock(&t.log);
            let offset = log.end();
            if offset == Offset::MAX {
                return Err(Refusal::Full {
                    what: "the topic's offsets",
                    limit: Offset::MAX,
                });
            }
            log.messages.push_back(Message {
                offset,
                key: key.map(<[u8]>::to_vec),
                payload: payload.to_vec(),
                at_us,
            });
            log.bytes = log.bytes.saturating_add(bytes);
            // The oldest go while the topic keeps more than its retention, in messages or bytes. The
            // newest stays: check_message refused a message its bytes can't keep alone.
            let retained = |log: &Log| u64::try_from(log.messages.len()).unwrap_or(u64::MAX);
            while (retained(&log) > t.topic.retain_messages || log.bytes > t.topic.retain_bytes)
                && log.messages.len() > 1
            {
                let Some(gone) = log.messages.pop_front() else {
                    break;
                };
                let size = gone
                    .key
                    .as_ref()
                    .map_or(0, Vec::len)
                    .saturating_add(gone.payload.len());
                log.bytes = log
                    .bytes
                    .saturating_sub(u64::try_from(size).unwrap_or(u64::MAX));
                log.first = log.first.saturating_add(1);
            }
            offset
        };
        t.changed.notify(usize::MAX);
        Ok(offset)
    }

    fn read_now(&self, name: &str, from: Offset, limit: usize) -> Result<Batch, Refusal> {
        let limit = check_limit(limit)?;
        let t = self.topic(name)?;
        let log = lock(&t.log);
        if from < log.first {
            return Err(Refusal::Behind {
                asked: from,
                first: log.first,
            });
        }
        let skip = usize::try_from(from.saturating_sub(log.first)).unwrap_or(usize::MAX);
        let messages: Vec<Message> = log
            .messages
            .iter()
            .skip(skip)
            .take(limit)
            .cloned()
            .collect();
        let next = messages
            .last()
            .map_or_else(|| from.min(log.end()), |m| m.offset.saturating_add(1));
        Ok(Batch { messages, next })
    }

    fn delete_now(&self, name: &str) -> Result<(), Refusal> {
        let t = lock(&self.inner.topics)
            .remove(name)
            .ok_or_else(|| Refusal::NoTopic(name.to_owned()))?;
        t.deleted.store(true, Ordering::Release);
        t.changed.notify(usize::MAX);
        Ok(())
    }
}

impl Topics for SimTopics {
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
        let out = self.publish_now(name, key, payload);
        Box::pin(async move { out })
    }

    fn read<'a>(
        &'a self,
        name: &'a str,
        from: Offset,
        limit: usize,
    ) -> BoxFuture<'a, Result<Batch, Refusal>> {
        let out = self.read_now(name, from, limit);
        Box::pin(async move { out })
    }

    fn wait<'a>(&'a self, name: &'a str, from: Offset) -> BoxFuture<'a, Result<(), Refusal>> {
        let topic = self.topic(name);
        Box::pin(async move {
            let t = topic?;
            loop {
                // Listening first, then looking: a publish between the two is still heard.
                let listener = t.changed.listen();
                if t.deleted.load(Ordering::Acquire) {
                    return Err(Refusal::NoTopic(name.to_owned()));
                }
                if lock(&t.log).end() > from {
                    return Ok(());
                }
                listener.await;
            }
        })
    }

    fn bounds<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(Offset, Offset), Refusal>> {
        let out = self.topic(name).map(|t| {
            let log = lock(&t.log);
            (log.first, log.end())
        });
        Box::pin(async move { out })
    }

    fn subscribe<'a>(
        &'a self,
        name: &'a str,
        start: Start,
    ) -> BoxFuture<'a, Result<Box<dyn Subscription>, Refusal>> {
        let out = self.topic(name).map(|topic| {
            let next = {
                let log = lock(&topic.log);
                start.at(log.first, log.end())
            };
            Box::new(SimSubscription {
                topic,
                next,
                held: None,
                listener: None,
            }) as Box<dyn Subscription>
        });
        Box::pin(async move { out })
    }
}

/// A subscription to a simulated topic.
struct SimSubscription {
    topic: Arc<SimTopic>,
    next: Offset,
    /// The message last taken, lent to the caller until the next take.
    held: Option<Message>,
    listener: Option<EventListener>,
}

impl SimSubscription {
    /// Whether a take has something now: the end, a lag notice or a message.
    fn has_next(&self) -> bool {
        if self.topic.deleted.load(Ordering::Acquire) {
            return true;
        }
        let log = lock(&self.topic.log);
        self.next < log.first || self.next < log.end()
    }
}

impl Subscription for SimSubscription {
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<()> {
        if self.has_next() {
            self.listener = None;
            return Poll::Ready(());
        }
        let listener = self
            .listener
            .get_or_insert_with(|| self.topic.changed.listen());
        match Pin::new(listener).poll(cx) {
            // Heard: what was published is there (a take sees it, or the end).
            Poll::Ready(()) => {
                self.listener = None;
                if self.has_next() {
                    return Poll::Ready(());
                }
                // Nothing for it after all: listen again, and be polled again to wait on it.
                self.listener = Some(self.topic.changed.listen());
                cx.waker().wake_by_ref();
                Poll::Pending
            }
            // Listening: a publish after this registration wakes it. One that came between the
            // check above and the listen is caught here, now the listener is in place.
            Poll::Pending if self.has_next() => {
                self.listener = None;
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn take(&mut self) -> Option<Next<'_>> {
        if self.topic.deleted.load(Ordering::Acquire) {
            self.held = None;
            return Some(Next::Ended);
        }
        let message = {
            let log = lock(&self.topic.log);
            if self.next < log.first {
                let from = self.next;
                self.next = log.first;
                return Some(Next::Lag {
                    from,
                    to: log.first,
                });
            }
            let index = usize::try_from(self.next.checked_sub(log.first)?).ok()?;
            log.messages.get(index)?.clone()
        };
        self.next = message.offset.checked_add(1)?;
        let held = self.held.insert(message);
        Some(Next::Message(MessageRef {
            offset: held.offset,
            key: held.key.as_deref(),
            payload: &held.payload,
            at_us: held.at_us,
        }))
    }

    fn position(&self) -> Offset {
        self.next
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conformance::{SimFixture, cases, run};

    /// QUIVER-1 to QUIVER-19: the simulator passes the suite, every clause.
    #[test]
    fn the_simulator_passes_the_topics_suite() -> Result<(), String> {
        let passed = futures::executor::block_on(run("Topics", &cases::<SimFixture>(), || async {
            SimFixture(SimTopics::new())
        }))
        .into_result()?;
        assert_eq!(passed, cases::<SimFixture>().len());
        Ok(())
    }

    /// Messages are stamped by the simulator's clock, which a test moves.
    #[test]
    fn messages_are_stamped_by_the_simulators_clock() {
        let sim = SimTopics::new();
        let topic = Topic {
            class: crate::Class::Ephemeral,
            retain_messages: 10,
            retain_bytes: 100,
        };
        assert!(futures::executor::block_on(sim.create("t", topic)).is_ok());
        sim.advance(Duration::from_millis(1500));
        assert!(futures::executor::block_on(sim.publish("t", None, b"x")).is_ok());
        let read = futures::executor::block_on(sim.read("t", 0, 1)).unwrap();
        assert_eq!(
            read.messages.first().map(|m| m.at_us),
            Some(SIM_EPOCH_US + 1_500_000)
        );
    }
}
