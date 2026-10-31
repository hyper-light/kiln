//! U14: typed process-local event topics on Quiver. There is no second message queue: each
//! subscriber owns an offset, and Quiver owns the retained payloads and their memory budget.

use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use futures::FutureExt as _;
use kiln_seams::bus::{Bus, Class, Refusal, Topic};
use serde::{Serialize, de::DeserializeOwned};

static NEXT: AtomicU64 = AtomicU64::new(1);

/// Why an event consumer must stop or recover from its canonical snapshot.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RecvError {
    #[error("the event publisher closed")]
    Closed,
    #[error("the consumer missed {0} event(s); refresh its snapshot")]
    Overflowed(u64),
    #[error("the event bus is unavailable: {0}")]
    Unavailable(String),
}

fn immediate<T>(f: futures::future::BoxFuture<'_, Result<T, Refusal>>) -> Result<T, Refusal> {
    f.now_or_never().unwrap_or_else(|| Err(Refusal::Unavailable("the ephemeral bus did not answer at once".into())))
}

/// Standalone runner/terminal users share one bounded node. Server events can explicitly use
/// their configured node through `Broadcast::on`, alongside the sessions' live topics.
fn local_bus() -> Result<Arc<dyn Bus>, Refusal> {
    static BUS: OnceLock<Result<Arc<dyn Bus>, String>> = OnceLock::new();
    BUS.get_or_init(|| super::QuiverBus::open(&super::Config::default(), crate::rt::clock())
        .map(|b| Arc::new(b) as Arc<dyn Bus>)
        .map_err(|e| format!("{e:#}")))
        .as_ref().map(Arc::clone).map_err(|e| Refusal::Unavailable(e.clone()))
}

struct Shared {
    bus: Result<Arc<dyn Bus>, Refusal>,
    topic: String,
    readers: AtomicUsize,
    closed: AtomicBool,
    /// Refused publishes have no offset. Notify consumers explicitly so they resnapshot rather
    /// than silently treating the next successful event as a complete history.
    refused: AtomicU64,
    changed: event_listener::Event,
}

impl Drop for Shared {
    fn drop(&mut self) {
        if let Ok(bus) = &self.bus
            && let Err(e) = immediate(bus.delete(&self.topic))
        {
            tracing::warn!(target: "kiln::bus", event = "topic.delete_refused", topic = %self.topic, error = %e, "event topic could not be deleted");
        }
    }
}

/// One publisher and any number of offset-based subscribers. Dropping the publisher wakes
/// subscribers; retained final messages are drained before they receive `Closed`.
pub struct Broadcast<T> {
    shared: Arc<Shared>,
    value: PhantomData<fn() -> T>,
}

impl<T> Broadcast<T> {
    pub fn new(capacity: usize) -> Self {
        Self::make(local_bus(), capacity)
    }

    pub fn on(bus: Arc<dyn Bus>, capacity: usize) -> Result<Self, Refusal> {
        let out = Self::make(Ok(bus), capacity);
        out.shared.bus.as_ref().map_err(Clone::clone)?;
        Ok(out)
    }

    fn make(bus: Result<Arc<dyn Bus>, Refusal>, capacity: usize) -> Self {
        let topic = format!("events/local/{}", NEXT.fetch_add(1, Ordering::Relaxed));
        let bus = bus.and_then(|bus| {
            immediate(bus.create(&topic, Topic { class: Class::Ephemeral, retain: capacity.clamp(1, kiln_seams::bus::RETAIN_MAX) }))?;
            Ok(bus)
        });
        if let Err(e) = &bus {
            tracing::warn!(target: "kiln::bus", event = "topic.create_refused", %topic, error = %e, "event topic could not be created");
        }
        Self { shared: Arc::new(Shared { bus, topic, readers: AtomicUsize::new(0), closed: AtomicBool::new(false), refused: AtomicU64::new(0), changed: event_listener::Event::new() }), value: PhantomData }
    }

    pub fn receiver_count(&self) -> usize {
        self.shared.readers.load(Ordering::Acquire)
    }

    pub fn subscribe(&self) -> Receiver<T> {
        self.shared.readers.fetch_add(1, Ordering::AcqRel);
        let position = self.shared.bus.as_ref().map_err(Clone::clone)
            .and_then(|bus| immediate(bus.bounds(&self.shared.topic)).map(|(_, next)| next));
        let (next, error) = match position {
            Ok(next) => (next, None),
            Err(e) => (0, Some(e.to_string())),
        };
        Receiver { shared: Arc::clone(&self.shared), next, refused: self.shared.refused.load(Ordering::Acquire), error, value: PhantomData }
    }
}

impl<T: Serialize> Broadcast<T> {
    pub fn send(&self, value: T) -> Result<(), Refusal> {
        if self.receiver_count() == 0 {
            return Ok(());
        }
        let out = (|| {
            let bus = self.shared.bus.as_ref().map_err(Clone::clone)?;
            let payload = postcard::to_allocvec(&value).map_err(|e| Refusal::Unavailable(format!("encode event: {e}")))?;
            if payload.len() > kiln_seams::bus::PAYLOAD_MAX {
                return Err(Refusal::TooLarge { what: "an event", size: payload.len(), limit: kiln_seams::bus::PAYLOAD_MAX });
            }
            immediate(bus.publish(&self.shared.topic, None, payload)).map(|_| ())
        })();
        if let Err(e) = &out {
            self.shared.refused.fetch_add(1, Ordering::AcqRel);
            self.shared.changed.notify(usize::MAX);
            tracing::warn!(target: "kiln::bus", event = "event.publish_refused", topic = %self.shared.topic, error = %e, "event refused; consumers must resnapshot");
        }
        out
    }
}

impl<T> Drop for Broadcast<T> {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
        self.shared.changed.notify(usize::MAX);
    }
}

pub struct Receiver<T> {
    shared: Arc<Shared>,
    next: u64,
    refused: u64,
    error: Option<String>,
    value: PhantomData<fn() -> T>,
}

impl<T: DeserializeOwned> Receiver<T> {
    pub async fn recv(&mut self) -> Result<T, RecvError> {
        if let Some(error) = self.error.take() {
            return Err(RecvError::Unavailable(error));
        }
        let bus = self.shared.bus.as_ref().map_err(|e| RecvError::Unavailable(e.to_string()))?;
        loop {
            // Listen before reading: close or a refused publish between the read and the wait
            // cannot strand a subscriber. Quiver's wait independently closes its publish race.
            let changed = self.shared.changed.listen();
            let refused = self.shared.refused.load(Ordering::Acquire);
            if refused != self.refused {
                let missed = refused.saturating_sub(self.refused);
                self.refused = refused;
                // A resnapshot supersedes the old pending events. Do not replay stale state
                // behind that snapshot after a publish was refused without taking an offset.
                self.next = bus.bounds(&self.shared.topic).await.map_err(|e| RecvError::Unavailable(e.to_string()))?.1;
                return Err(RecvError::Overflowed(missed));
            }
            match bus.read(&self.shared.topic, self.next, 1).await {
                Ok(batch) => {
                    if let Some(message) = batch.messages.into_iter().next() {
                        self.next = batch.next;
                        return postcard::from_bytes(&message.payload).map_err(|e| RecvError::Unavailable(format!("decode event: {e}")));
                    }
                    if self.shared.closed.load(Ordering::Acquire) {
                        return Err(RecvError::Closed);
                    }
                }
                Err(Refusal::Behind { first, .. }) => {
                    let missed = first.saturating_sub(self.next);
                    self.next = first;
                    return Err(RecvError::Overflowed(missed));
                }
                Err(e) => return Err(RecvError::Unavailable(e.to_string())),
            }
            let available = bus.wait(&self.shared.topic, self.next).fuse();
            let changed = changed.fuse();
            futures::pin_mut!(available, changed);
            futures::select! {
                result = available => {
                    if let Err(e) = result {
                        match e {
                            Refusal::Behind { first, .. } => {
                                let missed = first.saturating_sub(self.next);
                                self.next = first;
                                return Err(RecvError::Overflowed(missed));
                            }
                            e => return Err(RecvError::Unavailable(e.to_string())),
                        }
                    }
                },
                _ = changed => {},
            }
        }
    }
}

impl<T> Drop for Receiver<T> {
    fn drop(&mut self) {
        self.shared.readers.fetch_sub(1, Ordering::AcqRel);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    #[test]
    fn u14_application_events_keep_their_payloads() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let ui = Broadcast::<Arc<str>>::new(2);
            let mut ui_rx = ui.subscribe();
            ui.send(Arc::from(crate::wire::events::ServerEvent::Resync.text()))?;
            assert_eq!(&*ui_rx.recv().await?, r#"{"type":"resync"}"#);
            let logs = Broadcast::<crate::logs::LogEvent>::new(2);
            let mut log_rx = logs.subscribe();
            logs.send(crate::logs::LogEvent::Data { idx: 7, data: bytes::Bytes::from_static(b"output") })?;
            assert!(matches!(log_rx.recv().await?, crate::logs::LogEvent::Data { idx: 7, data } if data == b"output"[..]));
            let terminal = Broadcast::<crate::termsess::Frame>::new(2);
            let mut terminal_rx = terminal.subscribe();
            terminal.send(crate::termsess::Frame::Exit { code: Some(3), error: Some("ended".into()) })?;
            drop(terminal);
            assert!(matches!(terminal_rx.recv().await?, crate::termsess::Frame::Exit { code: Some(3), error: Some(e) } if e == "ended"));
            assert!(matches!(terminal_rx.recv().await, Err(RecvError::Closed)));
            Ok(())
        })
    }

    #[test]
    fn u14_quiver_fanout_orders_events_reports_lag_and_drains_on_close() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let bus: Arc<dyn Bus> = Arc::new(super::super::QuiverBus::open(&super::super::Config::default(), crate::rt::clock())?);
            let events = Broadcast::<u32>::on(Arc::clone(&bus), 2)?;
            let topic = events.shared.topic.clone();
            let mut fast = events.subscribe();
            let mut slow = events.subscribe();
            for n in 1..=3 {
                events.send(n)?;
                assert_eq!(fast.recv().await?, n);
            }
            assert_eq!(slow.recv().await, Err(RecvError::Overflowed(1)));
            drop(events);
            assert_eq!(slow.recv().await?, 2);
            assert_eq!(slow.recv().await?, 3);
            assert_eq!(slow.recv().await, Err(RecvError::Closed));
            assert_eq!(fast.recv().await, Err(RecvError::Closed));
            drop(fast);
            drop(slow);
            assert!(matches!(bus.bounds(&topic).await, Err(Refusal::NoTopic(_))));
            Ok(())
        })
    }

    #[test]
    fn u14_close_wakes_a_waiting_consumer_and_refused_events_require_resnapshot() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let events = Broadcast::<Vec<u8>>::new(2);
            let mut receiver = events.subscribe();
            assert!(events.send(vec![0; kiln_seams::bus::PAYLOAD_MAX + 1]).is_err());
            assert_eq!(receiver.recv().await, Err(RecvError::Overflowed(1)));
            let closed = crate::rt::spawn(async move { receiver.recv().await });
            crate::rt::sleep(Duration::from_millis(1)).await;
            drop(events);
            assert_eq!(crate::rt::timeout(Duration::from_secs(1), closed).await??, Err(RecvError::Closed));
            Ok(())
        })
    }
}
