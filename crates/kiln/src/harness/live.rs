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

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Instant;

use futures::FutureExt as _;
use kiln_seams::Bus;
use kiln_seams::bus::{Class, Offset, Refusal, Topic};

use super::registry::Frame;

/// Frames a session's topic keeps: a viewer further behind starts over from a snapshot.
pub const RETAIN: usize = 2048;

/// The marker a feed publishes after a frame the bus refused: its viewers start over.
pub const RESYNC: &str = r#"{"t":"resync"}"#;

/// A session's last frame: its viewers end with it.
pub const CLOSED: &str = r#"{"t":"closed"}"#;


/// Feeds made in this process: each takes the next, so a session's mirror made again (a restart
/// of its machine's link) has a topic of its own and never shares one it could delete.
static FEEDS: AtomicU64 = AtomicU64::new(0);

/// The topic of feed `n` of session `id`: `sessions/<id>/<n>`, or, for an id outside a topic
/// name's alphabet, its SHA-256 in its place.
pub fn topic(id: &str, n: u64) -> String {
    let plain = !id.is_empty() && id.len() <= 96 && id.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b'.'));
    if plain { format!("sessions/{id}/{n}") } else { format!("sessions/sha256-{}/{n}", kiln_seams::sha256_hex(id.as_bytes())) }
}

/// Microseconds on this process's monotonic clock: a frame's publish and its read are both here
/// (the feed's topic is this process's), so their difference is its delivery, finer than the
/// bus's millisecond stamps.
pub fn now_us() -> u64 {
    static BASE: OnceLock<Instant> = OnceLock::new();
    u64::try_from(BASE.get_or_init(Instant::now).elapsed().as_micros()).unwrap_or(u64::MAX)
}

/// A frame as the bus carries it: when it was published ([`now_us`], 8 bytes, little-endian),
/// its whole text's length (4 bytes, little-endian), the text, then what changed in a state when
/// there's a patch (the rest). The topic is this process's alone, so the form is free to change.
pub fn encode(frame: &Frame) -> Vec<u8> {
    let patch = frame.patch.as_deref().unwrap_or_default();
    let mut out = Vec::with_capacity(frame.text.len().saturating_add(patch.len()).saturating_add(12));
    out.extend_from_slice(&now_us().to_le_bytes());
    out.extend_from_slice(&u32::try_from(frame.text.len()).unwrap_or(u32::MAX).to_le_bytes());
    out.extend_from_slice(frame.text.as_bytes());
    out.extend_from_slice(patch.as_bytes());
    out
}

/// When the frame in `bytes` was published ([`now_us`]); `None` for bytes [`encode`] didn't make.
pub fn published_us(bytes: &[u8]) -> Option<u64> {
    bytes.first_chunk::<8>().map(|at| u64::from_le_bytes(*at))
}

/// A frame from what [`encode`] made; `None` for bytes it didn't make.
pub fn decode(bytes: &[u8]) -> Option<Frame> {
    let (_, rest) = bytes.split_first_chunk::<8>()?;
    let (len, rest) = rest.split_first_chunk::<4>()?;
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

/// What a feed shares with its viewers.
struct Shared {
    bus: Arc<dyn Bus>,
    topic: String,
    /// Viewers attached now, each counted by its [`Viewer`].
    viewers: AtomicUsize,
    /// Frames and topics the bus refused the feed: a viewer whose feed was refused since it last
    /// started over starts over again, whether or not a resync marker ever reaches it (a full bus
    /// may refuse that too).
    refusals: AtomicU64,
    /// The session ended (its feed dropped): the last of its viewers to go deletes the topic.
    ended: AtomicBool,
    /// The topic, made, was deleted: by whichever of the ended feed and its last viewer came first.
    deleted: AtomicBool,
}

impl Shared {
    /// Deletes the topic, once.
    fn delete(&self) {
        if !self.deleted.swap(true, Ordering::AcqRel) {
            delete(self.bus.as_ref(), &self.topic);
        }
    }
}

fn delete(bus: &dyn Bus, topic: &str) {
    if let Err(e) = at_once(bus.delete(topic)) {
        crate::metrics::bus().refused.inc(&["delete"]);
        // No viewer loses anything (none watches, or the session ended): info, where a viewer
        // sent back to a snapshot warns.
        tracing::info!(target: "kiln.bus", event = "feed.refused", topic = %topic, error = %e, "a session's live feed couldn't be let go");
    }
}

/// One session's live feed, as its mirror holds it. Dropped (its mirror let go: the session
/// ended or went away), its topic goes once its viewers have: each reads on to the session's last
/// frame ([`CLOSED`]) and ends with it, so none misses the frames it hadn't read yet.
pub struct Feed {
    shared: Arc<Shared>,
    /// A frame was refused: the next publish goes after [`RESYNC`]. Set too while the topic
    /// couldn't be made (it's asked again at the next publish).
    broken: bool,
    made: bool,
}

/// One viewer's place in a feed's count (and in `kiln_bus_viewers`); dropping it gives the place
/// back, and the last viewer of an ended session takes its topic with it. It reads the feed's
/// refusals, so it can tell its feed broke without a frame saying so.
pub struct Viewer(Arc<Shared>);

impl Viewer {
    fn new(shared: &Arc<Shared>) -> Self {
        shared.viewers.fetch_add(1, Ordering::AcqRel);
        crate::metrics::bus_gauges().viewers.add(1);
        Self(Arc::clone(shared))
    }

    /// The feed's refusals so far: more than when the viewer last started over means the feed
    /// broke since.
    pub fn refusals(&self) -> u64 {
        self.0.refusals.load(Ordering::Acquire)
    }
}

impl Drop for Viewer {
    fn drop(&mut self) {
        // Its place, taken once as it was made, given back once.
        crate::metrics::bus_gauges().viewers.add(-1);
        if self.0.viewers.fetch_sub(1, Ordering::SeqCst) == 1 && self.0.ended.load(Ordering::SeqCst) {
            self.0.delete();
        }
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
        let shared = Shared { bus, topic, viewers: AtomicUsize::new(0), refusals: AtomicU64::new(0), ended: AtomicBool::new(false), deleted: AtomicBool::new(false) };
        let mut feed = Self { shared: Arc::new(shared), broken: false, made: false };
        feed.make();
        feed
    }

    fn make(&mut self) -> bool {
        if self.made {
            return true;
        }
        match at_once(self.shared.bus.create(&self.shared.topic, Topic { class: Class::Ephemeral, retain: RETAIN })) {
            Ok(()) => {
                self.made = true;
                self.shared.deleted.store(false, Ordering::Release);
            }
            Err(e) => {
                // Said once as the feed breaks; asked again at each frame, each refusal counted.
                if !self.broken {
                    tracing::warn!(target: "kiln.bus", event = "feed.refused", topic = %self.shared.topic, error = %e, "a session's live feed couldn't be made: its viewers get snapshots until it is");
                }
                self.broken = true;
                self.shared.refusals.fetch_add(1, Ordering::AcqRel);
                crate::metrics::bus().refused.inc(&["create"]);
            }
        }
        self.made
    }

    pub fn topic(&self) -> &str {
        &self.shared.topic
    }

    /// Viewers watching now.
    pub fn viewers(&self) -> usize {
        self.shared.viewers.load(Ordering::Acquire)
    }

    /// The feed's refusals so far (what a viewer starting over now compares against later).
    pub fn refusals(&self) -> u64 {
        self.shared.refusals.load(Ordering::Acquire)
    }

    /// A viewer's place, and the offset its snapshot is taken at: where it reads on from (none
    /// while the bus can't hold the feed). Taken under the mirrors' lock, as every publish is, so
    /// no frame comes between the snapshot and the offset.
    pub fn attach(&mut self) -> (Viewer, Option<Offset>) {
        (Viewer::new(&self.shared), self.position())
    }

    /// Where a viewer that starts over reads on from: the offset a snapshot taken now is at.
    pub fn position(&mut self) -> Option<Offset> {
        if !self.make() {
            return None;
        }
        at_once(self.shared.bus.bounds(&self.shared.topic)).ok().map(|(_, end)| end)
    }

    /// Publishes `frame` to the feed's viewers: none watching, nothing goes, and what the topic
    /// kept goes too (the next viewer starts from a snapshot, on the topic made again), so a
    /// session nobody watches holds none of the bus's memory.
    pub fn publish(&mut self, frame: &Frame) -> Published {
        if self.viewers() == 0 {
            if self.made {
                self.made = false;
                self.broken = false;
                self.shared.delete();
            }
            return Published::Unwatched;
        }
        if !self.make() {
            return Published::Refused;
        }
        let m = crate::metrics::bus();
        if self.broken {
            match at_once(self.shared.bus.publish(&self.shared.topic, None, encode(&Frame::of(RESYNC.to_owned())))) {
                Ok(_) => {
                    self.broken = false;
                    m.published.inc();
                }
                Err(e) => return self.refused(&e),
            }
        }
        match at_once(self.shared.bus.publish(&self.shared.topic, None, encode(frame))) {
            Ok(_) => {
                m.published.inc();
                Published::Sent
            }
            Err(e) => self.refused(&e),
        }
    }

    fn refused(&mut self, e: &Refusal) -> Published {
        self.broken = true;
        self.shared.refusals.fetch_add(1, Ordering::AcqRel);
        crate::metrics::bus().refused.inc(&["publish"]);
        tracing::warn!(target: "kiln.bus", event = "feed.refused", topic = %self.shared.topic, error = %e, "a frame the bus refused: the session's viewers start over at its next one");
        Published::Refused
    }
}

impl Drop for Feed {
    /// The session ended: its topic goes now if no viewer reads it, else with its last viewer.
    fn drop(&mut self) {
        if !self.made {
            return;
        }
        self.shared.ended.store(true, Ordering::SeqCst);
        if self.shared.viewers.load(Ordering::SeqCst) == 0 {
            self.shared.delete();
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
        assert!(decode(b"\x00\x00\x00\x00\x00\x00\x00\x00\x05\x00\x00\x00abc").is_none());
    }

    /// A frame carries when it was published, on this process's microsecond clock: what the
    /// feed's delivery is timed from.
    #[test]
    fn a_frame_carries_the_microsecond_it_was_published() {
        let before = now_us();
        let bytes = encode(&Frame::of(r#"{"t":"events","events":[]}"#.to_owned()));
        let after = now_us();
        let at = published_us(&bytes);
        assert!(at.is_some_and(|at| (before..=after).contains(&at)), "{at:?} not in {before}..={after}");
        assert_eq!(published_us(b"\x01\x02"), None);
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
    /// A feed nobody watches keeps none of the bus's memory: its next frame lets the topic go,
    /// and the next viewer finds it made again, from its start.
    #[test]
    fn a_feed_nobody_watches_lets_its_topic_go_and_a_viewer_makes_it_again() -> anyhow::Result<()> {
        let b = bus()?;
        let mut feed = Feed::open(Arc::clone(&b), "s1");
        let (viewer, _) = feed.attach();
        assert_eq!(feed.publish(&Frame::of("seen".to_owned())), Published::Sent);
        drop(viewer);
        assert_eq!(feed.publish(&Frame::of("unseen".to_owned())), Published::Unwatched);
        let topic = feed.topic().to_owned();
        assert_eq!(futures::executor::block_on(b.bounds(&topic)), Err(Refusal::NoTopic(topic.clone())));
        let (_viewer, at) = feed.attach();
        assert_eq!(at, Some(0));
        assert_eq!(feed.publish(&Frame::of("again".to_owned())), Published::Sent);
        let read = futures::executor::block_on(b.read(&topic, 0, 10))?;
        let texts: Vec<String> = read.messages.iter().filter_map(|m| decode(&m.payload)).map(|f| f.text.to_string()).collect();
        assert_eq!(texts, ["again"]);
        Ok(())
    }

    /// A viewer learns its feed broke from the feed's refusals, without a frame saying so: a full
    /// bus may refuse the resync marker as well as the frame, and the viewer must not wait on.
    #[test]
    fn a_viewer_learns_its_feed_broke_without_a_frame_saying_so() -> anyhow::Result<()> {
        let b = bus()?;
        let mut feed = Feed::open(Arc::clone(&b), "s1");
        let (viewer, _) = feed.attach();
        let before = viewer.refusals();
        assert_eq!(feed.refusals(), before);
        let huge = Frame::of("x".repeat(kiln_seams::bus::PAYLOAD_MAX + 1));
        assert_eq!(feed.publish(&huge), Published::Refused);
        assert_eq!(feed.publish(&huge), Published::Refused);
        assert!(viewer.refusals() > before, "the viewer sees the refusals");
        Ok(())
    }

    /// An ended session's topic stays while a viewer reads it, so the frames it hadn't read yet,
    /// the session's last ([`CLOSED`]) among them, still reach it; it goes with the last viewer.
    #[test]
    fn an_ended_sessions_topic_stays_until_its_last_viewer_goes() -> anyhow::Result<()> {
        let b = bus()?;
        let mut feed = Feed::open(Arc::clone(&b), "s1");
        let (first, at) = feed.attach();
        let (second, _) = feed.attach();
        let at = at.ok_or_else(|| anyhow::anyhow!("no feed"))?;
        assert_eq!(feed.publish(&Frame::of("last words".to_owned())), Published::Sent);
        assert_eq!(feed.publish(&Frame::of(CLOSED.to_owned())), Published::Sent);
        let topic = feed.topic().to_owned();
        drop(feed);
        let read = futures::executor::block_on(b.read(&topic, at, 10))?;
        let texts: Vec<String> = read.messages.iter().filter_map(|m| decode(&m.payload)).map(|f| f.text.to_string()).collect();
        assert_eq!(texts, ["last words", CLOSED]);
        drop(first);
        assert!(futures::executor::block_on(b.bounds(&topic)).is_ok(), "one viewer still reads");
        drop(second);
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
