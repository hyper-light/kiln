#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing,
        clippy::arithmetic_side_effects,
        clippy::disallowed_macros
    )
)]
//! What other code reaches Quiver through (CLAUDE.md §9): topics of messages, each message given
//! the topic's next offset in the order it was published. Anyone reads a topic from any offset it
//! still keeps ([`Topics::read`]), or follows it with a subscription ([`Topics::subscribe`]) that
//! takes each message in order as it comes. A topic keeps its newest messages, up to its retention
//! in messages and in bytes: a reader asking for what's gone is refused [`Refusal::Behind`], naming
//! where what's kept starts, and a subscription that falls behind is told the offsets it missed
//! ([`Next::Lag`], the typed lag notice) and goes on from the oldest kept. Publishing never waits
//! for a reader or a subscription, so one that stalls holds up no one but itself (kiln's CP-T2.1,
//! AC-T2.1.5).
//!
//! Each topic states its class, how its messages outlive failures (Focal's doc 33): ephemeral,
//! replicated, persisted, windowed. An implementation serves the classes it can and refuses the
//! others, typed ([`Refusal::Unsupported`]); it never serves one class as another.
//!
//! [`sim::SimTopics`] is the simulator: in memory, deterministic, every class served as ephemeral.
//! [`conformance`] is the suite every implementation passes, the simulator and Quiver's node alike,
//! clause by clause (`QUIVER-n`, docs/design/topics.md).

use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use futures::future::BoxFuture;

pub mod conformance;
pub mod sim;

/// A message's place in its topic: 0 for the first ever published.
pub type Offset = u64;

/// How a topic's messages outlive failures (Focal's doc 33, §4.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Class {
    /// In memory, lost with its node; what a reader misses is announced, never skipped silently.
    Ephemeral,
    /// In the memory of a quorum, on independent fates.
    Replicated,
    /// Synced to disk by a quorum.
    Persisted,
    /// Written in order, synced within a bounded window: high-rate telemetry.
    Windowed,
}

impl Class {
    /// Every class, in the order doc 33 lists them.
    pub const ALL: [Self; 4] = [
        Self::Ephemeral,
        Self::Replicated,
        Self::Persisted,
        Self::Windowed,
    ];

    /// Its name, as docs and refusals say it.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Ephemeral => "ephemeral",
            Self::Replicated => "replicated",
            Self::Persisted => "persisted",
            Self::Windowed => "windowed",
        }
    }
}

/// A topic as created: its class and what it keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Topic {
    pub class: Class,
    /// Messages kept at most: past it, the oldest go.
    pub retain_messages: u64,
    /// Bytes of messages kept at most (keys and payloads): past it, the oldest go.
    pub retain_bytes: u64,
}

/// Longest topic name.
pub const NAME_MAX: usize = 128;
/// Longest key.
pub const KEY_MAX: usize = 1024;
/// Largest payload: nats-server's `MAX_PAYLOAD_SIZE`, Focal's largest batch (Focal's doc 32, §4.3).
pub const PAYLOAD_MAX: usize = 1 << 20;
/// Most messages one read returns.
pub const READ_MAX: usize = 1000;
/// Most messages a topic may keep.
pub const RETAIN_MESSAGES_MAX: u64 = 1_000_000;
/// Most bytes a topic may keep: 4 GiB.
pub const RETAIN_BYTES_MAX: u64 = 1 << 32;

/// A message as read: its own copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Message {
    pub offset: Offset,
    pub key: Option<Vec<u8>>,
    pub payload: Vec<u8>,
    /// When it was published: its node's wall clock, µs since the Unix epoch.
    pub at_us: u64,
}

/// Messages read from an offset.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Batch {
    pub messages: Vec<Message>,
    /// Where the next read goes on from.
    pub next: Offset,
}

/// A message as a subscription takes it: borrowed from where its topic keeps it, until the
/// subscription takes the next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MessageRef<'a> {
    pub offset: Offset,
    pub key: Option<&'a [u8]>,
    pub payload: &'a [u8],
    /// When it was published: its node's wall clock, µs since the Unix epoch.
    pub at_us: u64,
}

impl MessageRef<'_> {
    /// Its own copy.
    #[must_use]
    pub fn to_message(&self) -> Message {
        Message {
            offset: self.offset,
            key: self.key.map(<[u8]>::to_vec),
            payload: self.payload.to_vec(),
            at_us: self.at_us,
        }
    }
}

/// What a subscription takes next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Next<'a> {
    /// The message at the subscription's offset; it goes on from the one after.
    Message(MessageRef<'a>),
    /// The typed lag notice: the messages at offsets `from..to` were let go before this
    /// subscription took them. It goes on from `to`, the oldest its topic keeps.
    Lag { from: Offset, to: Offset },
    /// Its topic was deleted, or its node closed: nothing more comes.
    Ended,
}

/// Where a subscription starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Start {
    /// The oldest message its topic keeps.
    Oldest,
    /// The next message published: none kept before it.
    Latest,
    /// An offset: the message there, or a lag notice first if it's gone; past the end, the end.
    At(Offset),
}

impl Start {
    /// The offset it starts at, for a topic keeping `first..end`.
    #[must_use]
    pub const fn at(self, first: Offset, end: Offset) -> Offset {
        match self {
            Self::Oldest => first,
            Self::Latest => end,
            Self::At(offset) if offset > end => end,
            Self::At(offset) => offset,
        }
    }
}

/// Why a request was refused. Every refusal is typed, and none leaves a topic changed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    NoTopic(String),
    /// A topic of that name exists with other settings.
    Exists(String),
    Invalid {
        what: &'static str,
        why: String,
    },
    TooLarge {
        what: &'static str,
        size: u64,
        limit: u64,
    },
    /// A bound of the implementation's is reached.
    Full {
        what: &'static str,
        limit: u64,
    },
    /// The messages asked for are gone: those kept start at `first`.
    Behind {
        asked: Offset,
        first: Offset,
    },
    /// A class this implementation doesn't serve (yet): it never serves one class as another.
    Unsupported(Class),
    /// The implementation is closed, or can't be reached: why.
    Unavailable(String),
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoTopic(name) => write!(f, "no topic {name:?}"),
            Self::Exists(name) => write!(f, "topic {name:?} exists with other settings"),
            Self::Invalid { what, why } => write!(f, "{what} isn't valid: {why}"),
            Self::TooLarge { what, size, limit } => {
                write!(f, "{what} is {size}, over its limit of {limit}")
            }
            Self::Full { what, limit } => write!(f, "{what} is at its bound ({limit})"),
            Self::Behind { asked, first } => write!(
                f,
                "offset {asked} is gone: the topic keeps messages from {first}"
            ),
            Self::Unsupported(class) => write!(f, "the {} class isn't served here", class.name()),
            Self::Unavailable(why) => write!(f, "unavailable: {why}"),
        }
    }
}

impl std::error::Error for Refusal {}

/// Topics of messages. Every method's future is `Send`, and none holds a lock across its awaits.
pub trait Topics: Send + Sync + 'static {
    /// Creates `name` (again with the same settings: nothing changes).
    fn create<'a>(&'a self, name: &'a str, topic: Topic) -> BoxFuture<'a, Result<(), Refusal>>;
    /// Deletes `name` and every message it keeps: its subscriptions take [`Next::Ended`], its
    /// waits complete refused [`Refusal::NoTopic`].
    fn delete<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(), Refusal>>;
    /// Publishes a message, returning its offset. Never waits for a reader or a subscription.
    fn publish<'a>(
        &'a self,
        name: &'a str,
        key: Option<&'a [u8]>,
        payload: &'a [u8],
    ) -> BoxFuture<'a, Result<Offset, Refusal>>;
    /// The messages from offset `from`, oldest first, at most `limit` ([`READ_MAX`] at most).
    fn read<'a>(
        &'a self,
        name: &'a str,
        from: Offset,
        limit: usize,
    ) -> BoxFuture<'a, Result<Batch, Refusal>>;
    /// Completes once a message at `from` or later is there (at once when one is).
    fn wait<'a>(&'a self, name: &'a str, from: Offset) -> BoxFuture<'a, Result<(), Refusal>>;
    /// The first offset kept and the next to be given.
    fn bounds<'a>(&'a self, name: &'a str) -> BoxFuture<'a, Result<(Offset, Offset), Refusal>>;
    /// Follows `name` from `start`: every message in order, a lag notice for what it missed.
    fn subscribe<'a>(
        &'a self,
        name: &'a str,
        start: Start,
    ) -> BoxFuture<'a, Result<Box<dyn Subscription>, Refusal>>;
}

/// One reader following a topic. Dropping it ends it.
pub trait Subscription: Send {
    /// Ready once [`Self::take`] has something for it: a message, a lag notice or the end;
    /// otherwise its waker is woken when there is.
    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<()>;
    /// What it takes next, if anything is there: never waits. A message is borrowed from where the
    /// topic keeps it until the next take.
    fn take(&mut self) -> Option<Next<'_>>;
    /// The offset it takes next.
    fn position(&self) -> Offset;
}

impl dyn Subscription + '_ {
    /// Completes once [`Subscription::take`] has something.
    pub fn ready(&mut self) -> Ready<'_> {
        Ready(self)
    }
}

/// The future of [`Subscription::poll_ready`].
#[must_use = "a future does nothing unless awaited"]
pub struct Ready<'a>(&'a mut dyn Subscription);

impl Future for Ready<'_> {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        self.get_mut().0.poll_ready(cx)
    }
}

// ----------------------------------------------------------------------------------- checks

/// Refuses a topic name outside 1 to [`NAME_MAX`] of `A-Z a-z 0-9 _ - . /`.
pub fn check_name(name: &str) -> Result<(), Refusal> {
    let ok = !name.is_empty()
        && name.len() <= NAME_MAX
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.' | b'/'));
    if ok {
        Ok(())
    } else {
        Err(Refusal::Invalid {
            what: "a topic name",
            why: format!("{name:?}: 1 to {NAME_MAX} of A-Z, a-z, 0-9, _ - . /"),
        })
    }
}

/// Refuses a topic's retention outside 1 to [`RETAIN_MESSAGES_MAX`] messages and 1 to
/// [`RETAIN_BYTES_MAX`] bytes.
pub fn check_topic(topic: &Topic) -> Result<(), Refusal> {
    if topic.retain_messages == 0 || topic.retain_messages > RETAIN_MESSAGES_MAX {
        return Err(Refusal::Invalid {
            what: "a topic's retention in messages",
            why: format!("{} (1 to {RETAIN_MESSAGES_MAX})", topic.retain_messages),
        });
    }
    if topic.retain_bytes == 0 || topic.retain_bytes > RETAIN_BYTES_MAX {
        return Err(Refusal::Invalid {
            what: "a topic's retention in bytes",
            why: format!("{} (1 to {RETAIN_BYTES_MAX})", topic.retain_bytes),
        });
    }
    Ok(())
}

/// The bytes a message counts against its topic's retention: its key's and its payload's. Refuses
/// a key over [`KEY_MAX`], a payload over [`PAYLOAD_MAX`], and a message the topic can't keep.
pub fn check_message(topic: &Topic, key: Option<&[u8]>, payload: &[u8]) -> Result<u64, Refusal> {
    let size = |n: usize| u64::try_from(n).unwrap_or(u64::MAX);
    let key_len = key.map_or(0, <[u8]>::len);
    if key_len > KEY_MAX {
        return Err(Refusal::TooLarge {
            what: "a key",
            size: size(key_len),
            limit: size(KEY_MAX),
        });
    }
    if payload.len() > PAYLOAD_MAX {
        return Err(Refusal::TooLarge {
            what: "a payload",
            size: size(payload.len()),
            limit: size(PAYLOAD_MAX),
        });
    }
    let bytes = size(key_len).saturating_add(size(payload.len()));
    if bytes > topic.retain_bytes {
        return Err(Refusal::TooLarge {
            what: "a message (its key and payload)",
            size: bytes,
            limit: topic.retain_bytes,
        });
    }
    Ok(bytes)
}

/// Refuses a read's limit of 0, and holds a larger one to [`READ_MAX`].
pub fn check_limit(limit: usize) -> Result<usize, Refusal> {
    if limit == 0 {
        return Err(Refusal::Invalid {
            what: "a read's limit",
            why: "it's 0".to_owned(),
        });
    }
    Ok(limit.min(READ_MAX))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// QUIVER-8: names are 1 to 128 of a stated alphabet, checked the same by every implementation.
    #[test]
    fn a_name_outside_its_alphabet_or_length_is_refused() {
        assert!(check_name("sessions/abc-1.live_2").is_ok());
        for bad in ["", "has space", "ünïcode", &"x".repeat(NAME_MAX + 1)] {
            assert!(
                matches!(check_name(bad), Err(Refusal::Invalid { .. })),
                "{bad:?}"
            );
        }
        assert!(check_name(&"x".repeat(NAME_MAX)).is_ok());
    }

    /// QUIVER-5, QUIVER-9: a message is counted by its key and payload, and refused past its
    /// limits or its topic's retention.
    #[test]
    fn a_message_counts_its_key_and_payload_and_is_refused_past_its_limits() {
        let topic = Topic {
            class: Class::Ephemeral,
            retain_messages: 10,
            retain_bytes: 8,
        };
        assert_eq!(check_message(&topic, Some(b"ab"), b"cde"), Ok(5));
        assert!(matches!(
            check_message(&topic, None, b"123456789"),
            Err(Refusal::TooLarge { limit: 8, .. })
        ));
        let wide = Topic {
            retain_bytes: RETAIN_BYTES_MAX,
            ..topic
        };
        assert!(matches!(
            check_message(&wide, Some(&[0; KEY_MAX + 1]), b""),
            Err(Refusal::TooLarge { what: "a key", .. })
        ));
        assert!(matches!(
            check_message(&wide, None, &vec![0; PAYLOAD_MAX + 1]),
            Err(Refusal::TooLarge {
                what: "a payload",
                ..
            })
        ));
    }

    /// A subscription's start, for a topic keeping 5..9: past the end it starts at the end.
    #[test]
    fn a_start_names_its_offset() {
        assert_eq!(Start::Oldest.at(5, 9), 5);
        assert_eq!(Start::Latest.at(5, 9), 9);
        assert_eq!(Start::At(7).at(5, 9), 7);
        assert_eq!(Start::At(2).at(5, 9), 2);
        assert_eq!(Start::At(12).at(5, 9), 9);
    }
}
