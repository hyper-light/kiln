//! A session's live feed on kiln's bus (docs/bus.md; CP-T2.1). What the server learns of a session
//! from the machine it runs on goes to every viewer watching it, on any device, through a topic of
//! its own on the bus: Quiver's ephemeral class, in the server's process. The session's mirror
//! publishes each frame (`super::registry`); each viewer reads the topic from the offset its
//! snapshot was taken at, so it misses no frame and sees none twice. A viewer that falls behind
//! what the topic keeps is refused `Behind` (the bus's typed lag notice) and starts over from a
//! snapshot, as one that fell behind always did, while the publish, and every other viewer, goes
//! on: nothing a viewer does holds up the mirror or another viewer.
//!
//! A frame the bus can't take (over its size, or the bus's memory full) is never dropped silently:
//! the feed marks itself broken, and its next publish goes after a resync marker that sends every
//! viewer back to a snapshot. A mirror let go (the session ended) drops its feed, and the feed's
//! topic goes with it.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use futures::FutureExt as _;
use kiln_seams::Bus;
use kiln_seams::bus::{Class, Offset, Refusal, Topic};

use super::registry::Frame;

/// Frames a session's topic keeps: a viewer further behind starts over from a snapshot.
pub const RETAIN: usize = 2048;

/// The marker a feed publishes after a frame the bus refused: its viewers start over.
pub const RESYNC: &str = r#"{"t":"resync"}"#;

/// Feeds made in this process: each takes the next, so a session's mirror made again (a restart
/// of its machine's link) has a topic of its own and never shares one it could delete.
static FEEDS: AtomicU64 = AtomicU64::new(0);

/// The topic of feed `n` of session `id`: `sessions/<id>/<n>`, or, for an id outside a topic
/// name's alphabet, its SHA-256 in its place.
pub fn topic(id: &str, n: u64) -> String {
    let plain = !id.is_empty() && id.len() <= 96 && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    if plain { format!("sessions/{id}/{n}") } else { format!("sessions/sha256-{}/{n}", kiln_seams::sha256_hex(id.as_bytes())) }
}

/// A frame as the bus carries it: its whole text's length (4 bytes, little-endian), the text,
/// then what changed in a state when there's a patch (the rest).
pub fn encode(frame: &Frame) -> Vec<u8> {
    let patch = frame.patch.as_deref().unwrap_or_default();
    let mut out = Vec::with_capacity(frame.text.len().saturating_add(patch.len()).saturating_add(4));
    out.extend_from_slice(&u32::try_from(frame.text.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(frame.text.as_bytes());
    out.extend_from_slice(patch.as_bytes());
    out
}

/// A frame from what [`encode`] made; `None` for bytes it didn't make.
pub fn decode(bytes: &[u8]) -> Option<Frame> {
    let (len, rest) = bytes.split_first_chunk::<4>()?;
    let len = usize::try_from(u32::from_le_bytes(*len)).ok()?;
    let text = std::str::from_utf8(rest.get(..len)?).ok()?;
    let patch = std::str::from_utf8(rest.get(len..)?).ok()?;
    Some(Frame { text: Arc::from(text), patch: (!patch.is_empty()).then(|| Arc::from(patch)) })
}

/// How a feed's publish went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Published {
    /// On the bus.
    Sent,
    /// No viewer watches: nothing was published.
    Unwatched,
    /// The bus refused it: the feed is marked broken, and its viewers start over at its next frame.
    Refused,
}

/// One session's live feed, as its mirror holds it. Dropped (its mirror let go: the session
/// ended or went away), its topic goes, and a viewer reading on finds it gone.
pub struct Feed {
    bus: Arc<dyn Bus>,
    topic: String,
    /// Viewers attached now, each counted by its [`Viewer`].
    viewers: Arc<AtomicUsize>,
    /// A frame was refused: the next publish goes after [`RESYNC`]. Set too while the topic
    /// couldn't be made (it's asked again at the next publish).
    broken: bool,
    made: bool,
}

/// One viewer's place in a feed's count (and in `kiln_bus_viewers`); dropping it gives the place
/// back.
pub struct Viewer(Arc<AtomicUsize>);

impl Viewer {
    fn new(count: &Arc<AtomicUsize>) -> Self {
        count.fetch_add(1, Ordering::AcqRel);
        crate::metrics::bus_gauges().viewers.add(1);
        Self(Arc::clone(count))
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        // Its place, taken once as it was made, given back once.
        self.0.fetch_sub(1, Ordering::AcqRel);
        crate::metrics::bus_gauges().viewers.add(-1);
    }
}

/// The bus answers a feed at once: Quiver's ephemeral class never waits, nor does the simulator.
/// An answer that would wait is a refusal here (counted, and the viewers resynced), never a frame
/// dropped silently.
fn at_once<T>(f: futures::future::BoxFuture<'_, Result<T, Refusal>>) -> Result<T, Refusal> {
    f.now_or_never().unwrap_or_else(|| Err(Refusal::Unavailable("the bus didn't answer at once".to_owned())))
}

impl Feed {
    /// A feed for session `id`, its topic made on `bus`.
    pub fn open(bus: Arc<dyn Bus>, id: &str) -> Self {
        let topic = topic(id, FEEDS.fetch_add(1, Ordering::Relaxed));
        let mut feed = Self { bus, topic, viewers: Arc::new(AtomicUsize::new(0)), broken: false, made: false };
        feed.make();
        feed
    }

    fn make(&mut self) -> bool {
        if self.made {
            return true;
        }
        match at_once(self.bus.create(&self.topic, Topic { class: Class::Ephemeral, retain: RETAIN })) {
            Ok(()) => self.made = true,
            Err(e) => {
                self.broken = true;
                crate::metrics::bus().refused.inc(&["create"]);
                tracing::warn!(target: "kiln.bus", event = "feed.refused", topic = %self.topic, error = %e, "a session's live feed couldn't be made: its viewers get snapshots until it is");
            }
        }
        self.made
    }

    pub fn topic(&self) -> &str {
        &self.topic
    }

    /// Viewers watching now.
    pub fn viewers(&self) -> usize {
        self.viewers.load(Ordering::Acquire)
    }

    /// A viewer's place, and the offset its snapshot is taken at: where it reads on from (none
    /// while the bus can't hold the feed). Taken under the mirrors' lock, as every publish is, so
    /// no frame comes between the snapshot and the offset.
    pub fn attach(&mut self) -> (Viewer, Option<Offset>) {
        (Viewer::new(&self.viewers), self.position())
    }

    /// Where a viewer that starts over reads on from: the offset a snapshot taken now is at.
    pub fn position(&mut self) -> Option<Offset> {
        if !self.make() {
            return None;
        }
        at_once(self.bus.bounds(&self.topic)).ok().map(|(_, end)| end)
    }

    /// Publishes `frame` to the feed's viewers: none watching, nothing goes.
    pub fn publish(&mut self, frame: &Frame) -> Published {
        if self.viewers() == 0 {
            return Published::Unwatched;
        }
        if !self.make() {
            return Published::Refused;
        }
        let m = crate::metrics::bus();
        if self.broken {
            match at_once(self.bus.publish(&self.topic, None, encode(&Frame::of(RESYNC.to_owned())))) {
                Ok(_) => {
                    self.broken = false;
                    m.published.inc();
                }
                Err(e) => return self.refused(&e),
            }
        }
        match at_once(self.bus.publish(&self.topic, None, encode(frame))) {
            Ok(_) => {
                m.published.inc();
                Published::Sent
            }
            Err(e) => self.refused(&e),
        }
    }

    fn refused(&mut self, e: &Refusal) -> Published {
        self.broken = true;
        crate::metrics::bus().refused.inc(&["publish"]);
        tracing::warn!(target: "kiln.bus", event = "feed.refused", topic = %self.topic, error = %e, "a frame the bus refused: the session's viewers start over at its next one");
        Published::Refused
    }
}

impl Drop for Feed {
    fn drop(&mut self) {
        if self.made
            && let Err(e) = at_once(self.bus.delete(&self.topic))
        {
            crate::metrics::bus().refused.inc(&["delete"]);
            tracing::warn!(target: "kiln.bus", event = "feed.refused", topic = %self.topic, error = %e, "a session's live feed couldn't be let go");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bus() -> anyhow::Result<Arc<dyn Bus>> {
        Ok(Arc::new(crate::bus::QuiverBus::open(&crate::bus::Config::default(), Arc::new(kiln_seams::clock::SimClock::new()))?))
    }

    /// A frame goes on the bus and comes back as it went, its patch with it; bytes it didn't make
    /// come back as none.
    #[test]
    fn a_frame_comes_back_from_the_bus_as_it_went() {
        let frames = [Frame::of(r#"{"t":"events","events":[]}"#.to_owned()), Frame { text: Arc::from(r#"{"t":"state","state":{"a":1}}"#), patch: Some(Arc::from(r#"{"t":"state_patch","set":{"a":1}}"#)) }];
        for f in frames {
            let back = decode(&encode(&f)).map(|b| (b.text.to_string(), b.patch.as_deref().map(str::to_owned)));
            assert_eq!(back, Some((f.text.to_string(), f.patch.as_deref().map(str::to_owned))));
        }
        assert!(decode(b"\x09\x00").is_none());
        assert!(decode(b"\x05\x00\x00\x00abc").is_none());
    }

    /// Topic names: a session's id where it fits a name, its SHA-256 where it doesn't; the bus
    /// takes both, and each feed of a session has its own.
    #[test]
    fn every_feed_has_a_topic_of_its_own_the_bus_takes() -> anyhow::Result<()> {
        assert_eq!(topic("0192f0c5d4a87b3e9f1a2b3c4d5e6f70", 7), "sessions/0192f0c5d4a87b3e9f1a2b3c4d5e6f70/7");
        let odd = topic("an id with spaces / and ünïcode", u64::MAX);
        assert!(odd.starts_with("sessions/sha256-") && odd.len() <= kiln_seams::bus::NAME_MAX, "{odd}");
        let b = bus()?;
        for name in [topic("plain-id", 0), odd] {
            futures::executor::block_on(b.create(&name, Topic { class: Class::Ephemeral, retain: RETAIN }))?;
        }
        let (one, two) = (Feed::open(Arc::clone(&b), "s1"), Feed::open(Arc::clone(&b), "s1"));
        assert_ne!(one.topic(), two.topic());
        Ok(())
    }

    /// No viewer, nothing published; a viewer reads from the offset it attached at; a frame the
    /// bus refuses marks the feed broken, and the next goes after the resync marker; dropped, the
    /// feed's topic goes.
    #[test]
    fn a_feed_publishes_only_to_viewers_and_a_refused_frame_sends_them_back_to_a_snapshot() -> anyhow::Result<()> {
        let b = bus()?;
        let mut feed = Feed::open(Arc::clone(&b), "s1");
        assert_eq!(feed.publish(&Frame::of("unwatched".to_owned())), Published::Unwatched);
        let (viewer, at) = feed.attach();
        assert_eq!(at, Some(0));
        assert_eq!(feed.publish(&Frame::of("first".to_owned())), Published::Sent);
        let huge = Frame::of("x".repeat(kiln_seams::bus::PAYLOAD_MAX + 1));
        assert_eq!(feed.publish(&huge), Published::Refused);
        assert_eq!(feed.publish(&Frame::of("after".to_owned())), Published::Sent);
        let read = futures::executor::block_on(b.read(feed.topic(), 0, 10))?;
        let texts: Vec<String> = read.messages.iter().map(|m| decode(&m.payload).map(|f| f.text.to_string()).unwrap_or_default()).collect();
        assert_eq!(texts, ["first", RESYNC, "after"]);
        drop(viewer);
        assert_eq!(feed.viewers(), 0);
        let topic = feed.topic().to_owned();
        drop(feed);
        assert_eq!(futures::executor::block_on(b.bounds(&topic)), Err(Refusal::NoTopic(topic)));
        Ok(())
    }
    /// CP-T2.1, AC-T2.1.5 in kiln: a session's feed with 100 viewers, one stalled. Every frame is
    /// published at once; the 99 that read as frames come take all 3000 (more than the feed keeps)
    /// in order; the stalled one, reading at last from where it attached, is refused `Behind` (the
    /// typed lag notice that sends a viewer back to a snapshot), naming where the feed starts.
    #[test]
    fn a_stalled_viewer_among_100_is_told_it_fell_behind_and_delays_no_other() -> anyhow::Result<()> {
        let b = bus()?;
        let mut feed = Feed::open(Arc::clone(&b), "s1");
        let mut viewers = Vec::new();
        for _ in 0..100 {
            let (viewer, at) = feed.attach();
            viewers.push((viewer, at.ok_or_else(|| anyhow::anyhow!("no feed"))?, Vec::<String>::new()));
        }
        let frames = 3000;
        for n in 0..frames {
            assert_eq!(feed.publish(&Frame::of(format!("frame {n}"))), Published::Sent, "frame {n} went at once");
            // The 99: each reads what's there, as its socket's loop does.
            for (_, at, got) in viewers.iter_mut().skip(1) {
                let read = futures::executor::block_on(b.read(feed.topic(), *at, 1000))?;
                got.extend(read.messages.iter().filter_map(|m| decode(&m.payload)).map(|f| f.text.to_string()));
                *at = read.next;
            }
        }
        let want: Vec<String> = (0..frames).map(|n| format!("frame {n}")).collect();
        for (i, (_, _, got)) in viewers.iter().enumerate().skip(1) {
            assert_eq!(got, &want, "viewer {i} keeping up");
        }
        let (_, stalled_at, _) = viewers.first().ok_or_else(|| anyhow::anyhow!("no viewers"))?;
        let refused = futures::executor::block_on(b.read(feed.topic(), *stalled_at, 1000));
        let first = u64::try_from(frames - RETAIN)?;
        assert_eq!(refused, Err(Refusal::Behind { asked: 0, first }));
        Ok(())
    }
}
