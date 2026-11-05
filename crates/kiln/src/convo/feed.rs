//! A store's feed to another (`docs/conversations.md`, "Replication"): a runner's store sends the
//! server's what it wrote after the number the server has, oldest first; the server takes each
//! batch in one transaction, with that number kept beside what it took, and says through where it
//! has it. Nothing a runner wrote is lost to a dropped link, a restart on either side or a runner
//! away for days: the server's number says where to go on, and the runner's store still has
//! everything after it.
//!
//! On the runner's link to the server, a lane of its own ([`proto::LANE_CONVO`], bulk frames):
//! the runner says hello with its store's id, the server answers with the number it has, and the
//! runner sends batches from there, [`WINDOW`] ahead of what the server has acknowledged. A batch
//! larger than a frame goes in parts.
//!
//! Each batch changes hands signed by both sides (docs/ci.md W27-R08), once each end has a
//! [`Custodian`]: the giver's hand-on of it, by the SHA-256 of its rows, goes with its first part,
//! and the taker checks what came against it before its store takes any of it: a batch that isn't
//! what its hand-on says is refused (the giver is told to go on from where the taker stands, and
//! sends it again), and the taker's receipt of what came records the break.

use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::future::BoxFuture;
use kiln_ci::identity::statement::Subject;
use serde::{Deserialize, Serialize};

use super::store::{Change, Changes, Store};
use super::telemetry::{PeerView, Report, ShipStats, ShipView};
use crate::proto;

/// Who signs for one end of a feed (docs/ci.md W27-R08; the Transfers plan's step 12): a giver,
/// its hand-on of each batch it sends; a taker, its receipt of each it takes, or would have.
pub trait Custodian: Send + Sync {
    /// Its signed hand-on of `thing` (a batch, by the SHA-256 of its rows) to the feed's taker, as
    /// JSON; none when it signs none now (no identity yet, or a taker that takes no custody).
    fn hand_on(&self, thing: Subject) -> BoxFuture<'_, Option<String>>;

    /// Its receipt, as the taker of `machine`'s feed, of `thing` (what came, by its SHA-256),
    /// following `hand_on` (the giver's, as the batch carried it): signed, and kept with it.
    fn received(&self, machine: &str, hand_on: &str, thing: Subject);
}

/// Where a feed's end keeps its [`Custodian`]: set once its identity can sign, read per batch, a copy swapped
/// in whole (`crate::rcu`; goal 5, plan 01a1a59c2aae, its top goal: an `RwLock` each batch read on its shard).
type Signs = Arc<crate::rcu::Rcu<Option<Arc<dyn Custodian>>>>;

/// A batch of `store`'s feed, following `from` and reading through `upto`, by the SHA-256 of its
/// rows as JSON (`sha256`): what a hand-on and its receipt are about.
pub fn batch_thing(store: &str, from: u64, upto: u64, sha256: &str) -> Subject {
    Subject { name: format!("conversation store {store} after {from} through {upto}"), digest: std::collections::BTreeMap::from([("sha256".to_owned(), sha256.to_owned())]), ..Subject::default() }
}

/// The SHA-256 a hand-on (a custody record's bundle, as JSON) says its batch has.
fn handed(hand_on: &str) -> Option<String> {
    crate::custody::statement_in(hand_on)?.subject.first()?.digest.get("sha256").cloned()
}

/// The feed's lane on the runner's link.
pub const LANE: u32 = proto::LANE_CONVO;
/// About how much a batch holds (a row larger than it goes alone).
pub const BATCH_BYTES: usize = 2 << 20;
/// A part of a batch, at the most: one bulk frame (4 MiB) with room to spare.
pub const PART: usize = 3 << 20;
/// Batches sent ahead of what the server has acknowledged.
pub const WINDOW: usize = 2;
/// A server that says nothing for this long, while batches wait for it, is asked again where it
/// stands (a part lost with a link, a batch it couldn't take).
pub const STALL: Duration = Duration::from_secs(30);
/// The most a batch reassembled on the server may hold.
const BATCH_MAX: usize = 256 << 20;

/// A message of the feed (JSON; a batch's bytes travel beside it).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum Msg {
    /// Runner to server, when the link comes up (and when the server says nothing for too long):
    /// its store. `restore`: the store is its copy of the server's (a mirror's), sent back for
    /// the server to restore its own from ([`Msg::Restore`]).
    Hello {
        store: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        restore: bool,
    },
    /// Server to runner: it has `store`'s feed through `rev`; send from there. `zip`: it takes
    /// batches compressed; `kept`: it takes record lines as they are kept, compressed
    /// ([`RecordRow::kept`](super::rows::RecordRow::kept)). A taker from before says neither,
    /// and gets them as they are. `about`: the batch it answers (by its `from`), one that didn't
    /// follow or didn't come whole (None: a hello's answer, or a taker from before).
    From {
        store: String,
        rev: u64,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        zip: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        kept: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        about: Option<u64>,
    },
    /// Runner to server: part `part` (from 0) of `parts` of a batch that follows `from` and reads
    /// through `upto` (its bytes: JSON [`Changes`], brotli-compressed when `zip`). `told`: where
    /// the server last told it to go on from; a runner that says so leaves out an answer about
    /// what it has sent since (a runner from before doesn't say, and goes on from every answer).
    Batch {
        store: String,
        from: u64,
        upto: u64,
        part: u32,
        parts: u32,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        zip: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        told: Option<u64>,
        /// The giver's signed hand-on of the batch, on its first part ([`Custodian`]): a giver from
        /// before, or one that signs none now, sends none, and a taker from before ignores it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        hand_on: Option<String>,
    },
    /// Server to runner: it has `store`'s feed through `rev`.
    Ack { store: String, rev: u64 },
    /// Server to a mirror: send back your copy of my store, which is `store` now (the one you
    /// copied was lost).
    Restore { store: String },
    /// Mirror to server: its copy `store` has been sent whole, through `rev`.
    Restored { store: String, rev: u64 },
    /// Runner to server, every few seconds while linked: what its store `store` (its own, or its
    /// copy of the server's) is doing, for the server's telemetry. A server from before takes it
    /// for nothing.
    Stats { store: String, report: Box<Report> },
}

impl Msg {
    /// The store it is about.
    pub fn store(&self) -> &str {
        match self {
            Msg::Hello { store, .. } | Msg::From { store, .. } | Msg::Batch { store, .. } | Msg::Ack { store, .. } | Msg::Restore { store } | Msg::Restored { store, .. } | Msg::Stats { store, .. } => store,
        }
    }
}

pub fn encode(m: &Msg) -> String {
    serde_json::to_string(m).unwrap_or_default()
}

pub fn decode(json: &str) -> Option<Msg> {
    serde_json::from_str(json).ok()
}

/// Compression quality for a batch on the wire: fast, and most of the gain.
const ZIP_QUALITY: u32 = 4;

/// brotli's buffer, both ways; and so a unit of a batch's decoding on the compute pool ([`decode_batch`]): one read
/// of the decompressor's, or hashing as much, a few microseconds, well inside a piece.
const ZIP_BUFFER: usize = 4096;

/// The brotli window a batch of `len` bytes is zipped with, as bits: the smallest that holds it
/// (`crate::util::brotli_window`), and at most a batch's own size (`BATCH_BYTES`, 2^21 bytes; a row larger
/// than a batch goes alone, in a batch of its own past it). The decoder on the other side ([`Decoding`]) grows
/// its ring buffer to the window as the output passes it: at 22 bits for every batch, a batch just past 2 MiB
/// took a 4 MiB ring buffer, and one being decoded held 5.2 MB of the server's live memory in the lab's
/// realistic profile (goal 11, plan 01a1a87b2e56 step 5). A window of the batch's size reaches every match
/// the batch's own bytes offer but those a large row's spill past it, and any brotli decoder reads it.
fn zip_window(len: usize) -> u32 {
    crate::util::brotli_window(len).min(BATCH_BYTES.ilog2())
}

fn zip(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    use std::io::Write as _;
    let mut out = Vec::with_capacity(bytes.len() / 4 + 16);
    {
        let mut w = brotli::CompressorWriter::new(&mut out, ZIP_BUFFER, ZIP_QUALITY, zip_window(bytes.len()));
        w.write_all(bytes)?;
        w.flush()?;
    }
    Ok(out)
}

/// A batch as the messages it travels in, compressed when the taker takes that (`zipped`), sent
/// after being told to go on from `told` ([`Msg::Batch`]).
pub fn parts(store: &str, from: u64, batch: &Changes, zipped: bool, told: Option<u64>) -> anyhow::Result<Vec<(Msg, Vec<u8>)>> {
    parts_of(store, from, batch.upto, serde_json::to_vec(batch)?, zipped, told, None)
}

/// [`parts`] of a batch read through `upto` as `json`, its giver's `hand_on` with its first part.
fn parts_of(store: &str, from: u64, upto: u64, json: Vec<u8>, zipped: bool, told: Option<u64>, hand_on: Option<String>) -> anyhow::Result<Vec<(Msg, Vec<u8>)>> {
    let bytes = if zipped { zip(&json)? } else { json };
    let chunks: Vec<&[u8]> = if bytes.is_empty() { vec![&[][..]] } else { bytes.chunks(PART).collect() };
    let n = u32::try_from(chunks.len())?;
    Ok(chunks
        .into_iter()
        .enumerate()
        .map(|(i, c)| (Msg::Batch { store: store.to_owned(), from, upto, part: u32::try_from(i).unwrap_or(u32::MAX), parts: n, zip: zipped, told, hand_on: hand_on.clone().filter(|_| i == 0) }, c.to_vec()))
        .collect())
}

// ------------------------------------------------------------------------------- the runner's side

/// What the runner's link tells the feed.
#[derive(Debug)]
pub enum Link {
    /// Connected to the server.
    Up,
    Down,
    /// The server said this.
    Heard(Msg),
    /// The store committed something.
    Wrote,
}

/// A feed's parts waiting for its link to take them, at the most: as many as the channel they waited in
/// held. The feed waits for room past them, so a slow link holds it back.
const OUT_PARTS: usize = 16;

/// What a feed hears of its link, lock-free (`crate::memory::queue`; goal 4, plan 01a1a59c2a9c step 5: an
/// async-channel's wait took event-listener's `Mutex`, the runner's link and its store's commits sending on
/// their shards and the feed waiting on its own). Unbounded, as that channel was: a link's word and a commit's
/// nudge never wait, and the feed takes each as it comes.
fn links() -> (crate::memory::queue::Sender<Link>, crate::memory::queue::Receiver<Link>) {
    crate::memory::queue::unbounded(&crate::memory::owners::CHANNELS)
}

/// A feed's parts to send, `(json, bytes)`: [`OUT_PARTS`] of them whatever each carries (a part up to
/// [`PART`]), lock-free (goal 4, plan 01a1a59c2a9c step 5, as [`links`]).
fn parts_out() -> (crate::memory::queue::Sender<(String, Vec<u8>)>, crate::memory::queue::Receiver<(String, Vec<u8>)>) {
    crate::memory::queue::places(&crate::memory::owners::CHANNELS, OUT_PARTS)
}

/// A runner's feed: tell it of the link with [`Shipper::link`]; what it sends comes out of the
/// receiver [`Shipper::start`] returns, as `(json, bytes)` for [`proto::AgentMsg::Convo`].
#[derive(Clone)]
pub struct Shipper {
    tx: crate::memory::queue::Sender<Link>,
    stats: std::sync::Arc<ShipStats>,
    signs: Signs,
}

/// Whether the taker has a row already (by its number): it is left out of what goes to it.
pub type Skip = std::sync::Arc<dyn Fn(u64) -> bool + Send + Sync>;

impl Shipper {
    pub fn start(store: Store) -> (Self, crate::memory::queue::Receiver<(String, Vec<u8>)>) {
        Self::start_as(store, false, None)
    }

    /// [`Shipper::start`], leaving out the rows `skip` says the taker has (what a mirror sent
    /// back to restore this store from isn't sent to it again, [`Echoes`]).
    pub fn start_skipping(store: Store, skip: Skip) -> (Self, crate::memory::queue::Receiver<(String, Vec<u8>)>) {
        Self::start_as(store, false, Some(skip))
    }

    /// A mirror's copy of the server's store, sent back for the server to restore its own from
    /// ([`Msg::Restore`]): everything the copy holds now, then [`Msg::Restored`], and the feed
    /// ends (what it takes meanwhile, it takes from the server: not sent back).
    pub fn start_restore(store: Store) -> (Self, crate::memory::queue::Receiver<(String, Vec<u8>)>) {
        Self::start_as(store, true, None)
    }

    fn start_as(store: Store, restore: bool, skip: Option<Skip>) -> (Self, crate::memory::queue::Receiver<(String, Vec<u8>)>) {
        let (tx, rx) = links();
        let (out_tx, out_rx) = parts_out();
        // Each commit, as a nudge in the same queue.
        let watch = store.watch();
        let nudge = tx.clone();
        // The feed's two tasks on the next shards round-robin, not the one that starts it: a server
        // starts every feed on the shard its start-up runs on (goal 9, plan 01a1a87b2cec step 3).
        crate::rt::spawn_spread(async move {
            while watch.recv().await.is_ok() {
                if nudge.send(Link::Wrote).await.is_err() {
                    break;
                }
            }
        });
        let stats: std::sync::Arc<ShipStats> = std::sync::Arc::default();
        let signs = Signs::default();
        crate::rt::spawn_spread(ship(store, rx, out_tx, restore, skip, stats.clone(), signs.clone()));
        (Self { tx, stats, signs }, out_rx)
    }

    pub fn link(&self, l: Link) {
        let _ = self.tx.try_send(l);
    }

    /// Its batches go signed by `c` from now on ([`Custodian`]).
    pub fn sign_with(&self, c: Arc<dyn Custodian>) {
        self.signs.set(Some(c));
    }

    /// Where the feed stands, for telemetry.
    pub fn stats(&self) -> ShipView {
        self.stats.view()
    }
}

/// Where the runner's feed stands on this link.
struct Shipping {
    /// What the server has, once it said (None: it hasn't yet, on this link).
    acked: Option<u64>,
    /// The server takes batches compressed (it said so).
    zipped: bool,
    /// It takes record lines as they are kept (it said so).
    kept: bool,
    /// Read and sent through here.
    sent: u64,
    /// The batches sent and not yet acknowledged, in the order sent: their `from` and `upto`.
    flight: VecDeque<(u64, u64)>,
    /// Where the server last told it to go on from (its batches say so).
    told: Option<u64>,
    /// When the server last said something, or was last asked.
    heard: Instant,
}

/// A test build's `feed_changed` changed a batch of this process's once already.
static CHANGED_ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

async fn ship(store: Store, rx: crate::memory::queue::Receiver<Link>, out: crate::memory::queue::Sender<(String, Vec<u8>)>, restore: bool, skip: Option<Skip>, stats: std::sync::Arc<ShipStats>, signs: Signs) {
    let me = store.id().to_owned();
    // A copy sent back goes through where it reads now, and no further.
    let through = if restore {
        let s = store.clone();
        match crate::rt::blocking(move || s.top()).await {
            Ok(Ok(top)) => Some(top),
            Ok(Err(e)) | Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "conversation feed: the copy to send back couldn't be read");
                return;
            }
        }
    } else {
        None
    };
    let mut up = false;
    let mut st = Shipping { acked: None, zipped: false, kept: false, sent: 0, flight: VecDeque::new(), told: None, heard: Instant::now() };
    let hello = |out: &crate::memory::queue::Sender<(String, Vec<u8>)>| out.try_send((encode(&Msg::Hello { store: me.clone(), restore }), Vec::new())).is_ok();
    // Its turns on its shard: what waits in its queue is taken without waiting (a commit's nudges pile up while its
    // window is full, 20 a second for as long as its taker is slow), so it's given up between them once its turn would
    // pass the quantum (goal 9, plan 01a1a87b2cec step 3).
    let mut turns = crate::rt::Turns::new();
    loop {
        turns.after().await;
        let ev = match crate::rt::timeout(Duration::from_secs(1), rx.recv()).await {
            Ok(Ok(ev)) => Some(ev),
            Ok(Err(_)) => return,
            Err(_) => None,
        };
        match ev {
            Some(Link::Up) => {
                up = true;
                st = Shipping { acked: None, zipped: false, kept: false, sent: 0, flight: VecDeque::new(), told: None, heard: Instant::now() };
                stats.link(true);
                stats.hello(false);
                hello(&out);
            }
            Some(Link::Down) => {
                up = false;
                st.acked = None;
                st.flight.clear();
                stats.link(false);
            }
            Some(Link::Heard(Msg::From { store, rev, zip, kept, about })) if store == me => {
                // Asked to go on from where a batch of it, sent since, starts: an answer to one
                // sent before it went on from there (a batch already on its way when it was told,
                // or a hello's answer that came late). That batch follows; sending it again had
                // the server answer each copy the same, a storm that took nothing for minutes on a
                // busy server. Unless the server says it answers one of those sent since: those
                // before it were lost. Not hearing from the server, either: a lost batch still
                // ends in a hello (STALL).
                let since = |f: u64| st.flight.iter().any(|(from, _)| *from == f);
                if since(rev) && !about.is_some_and(since) {
                    continue;
                }
                // Where to go on from (a first answer, or after something went astray).
                st.acked = Some(rev);
                st.zipped = zip;
                st.kept = kept;
                st.sent = rev;
                st.told = Some(rev);
                st.flight.clear();
                st.heard = Instant::now();
                stats.from(rev);
            }
            Some(Link::Heard(Msg::Ack { store, rev })) if store == me => {
                st.acked = Some(rev);
                st.flight.retain(|(_, u)| *u > rev);
                st.heard = Instant::now();
                stats.acked(rev, st.flight.len());
            }
            Some(Link::Heard(_) | Link::Wrote) | None => {}
        }
        if !up {
            continue;
        }
        // Silence for too long, before or after sending: ask again where it stands.
        if (st.acked.is_none() || !st.flight.is_empty()) && st.heard.elapsed() >= STALL {
            st.acked = None;
            st.flight.clear();
            st.heard = Instant::now();
            stats.hello(true);
            hello(&out);
            continue;
        }
        if st.acked.is_none() {
            continue;
        }
        // A copy sent back: done once the server has it through where it read when it began.
        if let (Some(end), Some(acked)) = (through, st.acked)
            && acked >= end
            && st.flight.is_empty()
        {
            let _ = out.send((encode(&Msg::Restored { store: me.clone(), rev: end }), Vec::new())).await;
            return;
        }
        while st.flight.len() < WINDOW {
            // A copy sent back stops reading where it read when it began.
            if through.is_some_and(|end| st.sent >= end) {
                break;
            }
            let (from, kept) = (st.sent, st.kept);
            let s = store.clone();
            let mut batch = match crate::rt::blocking(move || s.changes_as(from, BATCH_BYTES, kept)).await {
                Ok(Ok(b)) => b,
                Ok(Err(e)) | Err(e) => {
                    tracing::debug!(error = %format!("{e:#}"), "conversation feed: the store couldn't be read");
                    break;
                }
            };
            if batch.rows.is_empty() {
                break;
            }
            // Its JSON and the SHA-256 of it made on the compute pool (goal 3, plan 01a1a59c2a8e step 3): a
            // batch of up to two MiB, written out and hashed on the shard, held it all the while. What the taker
            // has already is left out there first, the rows let go of there too: a mirror's batch asked of each
            // of its up to tens of thousands of rows whether it had it, on the shard (goal 9, plan 01a1a87b2cec
            // step 3). The batch still says through where it read.
            let (upto, skip) = (batch.upto, skip.clone());
            let encoded = crate::rt::compute(move || -> anyhow::Result<(Vec<u8>, String, usize)> {
                if let Some(skip) = &skip {
                    batch.rows.retain(|c| !skip(c.rev));
                }
                let json = serde_json::to_vec(&batch)?;
                let digest = crate::util::sha256_hex(&json);
                Ok((json, digest, batch.rows.len()))
            })
            .await;
            let (mut json, digest, rows) = match encoded {
                Ok(Ok(e)) => e,
                Ok(Err(e)) | Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "conversation feed: a batch couldn't be encoded");
                    break;
                }
            };
            // Its hand-on, by the SHA-256 of what goes (docs/ci.md W27-R08).
            let signer = Option::clone(&signs.load());
            let hand_on = match signer {
                Some(c) => c.hand_on(batch_thing(&me, from, upto, &digest)).await,
                None => None,
            };
            // A test build's `feed_changed`: the first batch signed for changes on its way, once.
            if hand_on.is_some()
                && crate::runner::chaos::get().is_some_and(|c| c.feed_changed)
                && !CHANGED_ONCE.swap(true, std::sync::atomic::Ordering::Relaxed)
                && let Some(b) = json.iter_mut().rev().find(|b| b.is_ascii_alphabetic())
            {
                *b ^= 0x20;
            }
            // Compressed and cut into its parts on the compute pool too.
            let (store_id, zipped, told) = (me.clone(), st.zipped, st.told);
            let msgs = match crate::rt::compute(move || parts_of(&store_id, from, upto, json, zipped, told, hand_on)).await {
                Ok(Ok(m)) => m,
                Ok(Err(e)) | Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "conversation feed: a batch couldn't be encoded");
                    break;
                }
            };
            let bytes: usize = msgs.iter().map(|(_, d)| d.len()).sum();
            for (m, data) in msgs {
                if out.send((encode(&m), data)).await.is_err() {
                    return;
                }
            }
            if st.flight.is_empty() {
                st.heard = Instant::now();
            }
            st.sent = upto;
            st.flight.push_back((from, upto));
            stats.sent(upto, rows, bytes, st.flight.len());
        }
    }
}

// ------------------------------------------------------------------------------- the server's side

/// Something to send the runner process `peer`: a [`Msg`] as JSON, for [`proto::ServerMsg::Convo`].
#[derive(Debug)]
pub struct Out {
    pub peer: hyper_transport::PeerId,
    pub json: String,
}

/// Where an intake stands with one store's feed from one runner process.
#[derive(Default)]
struct Taking {
    /// A batch being put back together from its parts.
    batch: Option<Partial>,
    /// Where it last told the runner to go on from, since a batch of the feed was last taken.
    told: Option<u64>,
}

/// The feeds an intake keeps where it stands with, at the most: past it, only those putting a
/// batch together are kept (what each was told is only to tell it once).
const TAKING_MAX: usize = 4096;

/// A batch being put back together from its parts.
struct Partial {
    from: u64,
    upto: u64,
    parts: u32,
    got: u32,
    zip: bool,
    bytes: Vec<u8>,
    /// Its giver's hand-on, from its first part.
    hand_on: Option<String>,
}

/// Where the sessions a feed brings run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Brings {
    /// On the machine whose feed it is: a runner's store, sent to the server's.
    TheirMachine,
    /// Where its rows say: the server's store, sent to a mirror.
    AsItSays,
}

/// The taking side: one thread takes what feeds send into a store, in the order each sent it, and
/// hands back what to answer. The server's takes runners' stores; a mirror's takes the server's.
pub struct Intake {
    tx: crate::memory::queue::Sender<Heard>,
    restores: std::sync::Arc<Restores>,
    /// Where each store that feeds it stands ([`Peers`]).
    peers: std::sync::Arc<Peers>,
    signs: Signs,
}

/// Where each store that feeds an intake stands, by its name among the store's peers ([`peer_name`]): for
/// telemetry. Its thread alone writes it, as it takes each message ([`Feed::note`]), and [`Intake::peers`]
/// reads it, with no lock (goal 5, plan 01a1a59c2aae, its top goal: a `Mutex` the thread took for each message
/// and a telemetry read took on its shard). Each store's view is a copy of its own, swapped in whole, and the
/// map of them a copy swapped in whole as a store first feeds it (`crate::rcu`): a message copies its own
/// store's view, never every store's.
type Peers = crate::rcu::Rcu<HashMap<String, Arc<crate::rcu::Rcu<PeerView>>>>;

/// A feed's message as its intake's thread takes it: the link it came on, its machine, the message, and
/// the bytes that came with it (a part of a batch).
type Heard = (hyper_transport::PeerId, String, Msg, Vec<u8>);

impl crate::memory::queue::Weigh for Heard {
    /// Its place, its machine's and store's names, a batch's hand-on, and the bytes it carries: a batch's
    /// part (up to [`PART`]) is all of it but those few names (a report's figures counted by its place).
    fn weight(&self) -> usize {
        let hand_on = match &self.2 {
            Msg::Batch { hand_on, .. } => hand_on.as_ref().map_or(0, String::len),
            Msg::Stats { .. } => size_of::<Report>(),
            _ => 0,
        };
        size_of::<Heard>().saturating_add(self.1.len()).saturating_add(self.2.store().len()).saturating_add(hand_on).saturating_add(self.3.len())
    }
}

/// An intake's answers waiting to go to their runners, at the most: as many as the channel they waited in
/// held. Its thread waits for room past them.
const ANSWERS: usize = 1024;

/// An intake's answers to its feeds, [`ANSWERS`] of them, lock-free (`crate::memory::queue`; goal 4, plan
/// 01a1a59c2a9c step 5: an async-channel's wait took event-listener's `Mutex`, the intake's thread sending and
/// the task that sends them on waiting on its shard).
fn answers() -> (crate::memory::queue::Sender<Out>, crate::memory::queue::Receiver<Out>) {
    crate::memory::queue::places(&crate::memory::owners::CHANNELS, ANSWERS)
}

/// The bytes of messages waiting for an intake's thread, at most (goal 11, plan 01a1a87b2e56 step 3): one
/// connection's receive window ([`crate::net::WINDOW`]), what the hub's queue of runner events it's handed
/// from may hold; a feed sends at most [`WINDOW`] batches of [`BATCH_BYTES`] ahead of the server, so it
/// holds every feed's window for as many feeds as fit in it. Past it a message is dropped, as it was past
/// 256 messages of any size (up to 768 MiB of parts): its runner, holding the batch until the server has
/// it, asks again ([`STALL`]). Nothing a person sent is lost.
pub const HEARD_BYTES: usize = crate::net::WINDOW as usize;

/// What each mirror sent back to restore a store, by the numbers it was written under there: the
/// feed to that mirror leaves it out, as the mirror has it already, in the copy it sent. Read for each
/// row a mirror's feed sends and written as a copy sent back is taken, so a copy swapped in whole
/// (`crate::rcu`; goal 5, plan 01a1a59c2aae, its top goal: a `Mutex` the feeds' shards and the intake's
/// thread took).
#[derive(Default)]
pub struct Echoes(crate::rcu::Rcu<HashMap<String, Vec<(u64, u64)>>>);

impl Echoes {
    /// Numbers `first` through `last` were written from `machine`'s copy.
    pub fn add(&self, machine: &str, first: u64, last: u64) {
        self.0.update(|all| {
            let ranges = all.entry(machine.to_owned()).or_default();
            match ranges.last_mut() {
                Some(r) if r.1 + 1 == first => r.1 = last,
                _ => ranges.push((first, last)),
            }
        });
    }

    /// Whether number `rev` was written from `machine`'s copy.
    pub fn has(&self, machine: &str, rev: u64) -> bool {
        self.0.load().get(machine).is_some_and(|ranges| {
            let i = ranges.partition_point(|r| r.1 < rev);
            ranges.get(i).is_some_and(|r| r.0 <= rev)
        })
    }
}

/// The mirrors' copies an intake takes to restore its store from ([`Intake::accept_restore`]): read for each
/// batch its thread takes, written as a mirror says it sends one back, so a copy swapped in whole
/// (`crate::rcu`; goal 5, plan 01a1a59c2aae, its top goal).
type Restores = crate::rcu::Rcu<std::collections::HashSet<String>>;

impl Intake {
    /// `echoes`: where to note what a mirror's copy brought (the server's intake).
    pub fn start(store: Store, brings: Brings, echoes: Option<std::sync::Arc<Echoes>>) -> anyhow::Result<(Self, crate::memory::queue::Receiver<Out>)> {
        let (tx, rx) = crate::memory::queue::bounded::<Heard>(&crate::memory::owners::FEED_INTAKE, HEARD_BYTES);
        let (out_tx, out_rx) = answers();
        let restores: std::sync::Arc<Restores> = std::sync::Arc::default();
        let peers: std::sync::Arc<Peers> = std::sync::Arc::default();
        let signs = Signs::default();
        let (restoring, seen, signing) = (restores.clone(), peers.clone(), signs.clone());
        std::thread::Builder::new().name("kiln-convo-intake".into()).spawn(move || {
            let mut taking: HashMap<(hyper_transport::PeerId, String), Taking> = HashMap::new();
            while let Ok((peer, machine, msg, data)) = rx.recv_blocking() {
                let signer = Option::clone(&signing.load());
                let feed = Feed { store: &store, brings, restores: &restoring, echoes: echoes.as_deref(), peers: &seen, custodian: signer.as_deref() };
                for m in take(&feed, &mut taking, peer, &machine, msg, data) {
                    if out_tx.send_blocking(Out { peer, json: encode(&m) }).is_err() {
                        return;
                    }
                }
            }
        })?;
        Ok((Self { tx, restores, peers, signs }, out_rx))
    }

    /// The batches it takes are signed for by `c` from now on ([`Custodian`]).
    pub fn sign_with(&self, c: Arc<dyn Custodian>) {
        self.signs.set(Some(c));
    }

    /// Where each store that feeds this intake stands, since kiln started.
    pub fn peers(&self) -> Vec<PeerView> {
        self.peers.load().values().map(|v| PeerView::clone(&v.load())).collect()
    }

    /// Take store `store`'s feed as a mirror's copy of this one, sent back to restore it
    /// ([`super::store::Store::take_copy`]): what it brings fills in what this store lacks.
    pub fn accept_restore(&self, store: &str) {
        self.restores.update(|all| {
            all.insert(store.to_owned());
        });
    }

    /// What runner process `peer` of `machine` sent on the feed's lane, read where its frame was
    /// (`crate::wire::payload_of`). Never blocks: when the intake is behind (its queue holds
    /// [`HEARD_BYTES`]), it's dropped (the runner asks again, [`STALL`]).
    pub fn heard(&self, peer: hyper_transport::PeerId, machine: &str, msg: Msg, data: Vec<u8>) {
        let _ = self.tx.try_send((peer, machine.to_owned(), msg, data));
    }
}

/// The name a store has among the server's peers: its machine's, then its own.
pub fn peer_name(machine: &str, store: &str) -> String {
    format!("{machine}/{store}")
}

/// What an intake takes into, and keeps of what it takes.
struct Feed<'a> {
    store: &'a Store,
    brings: Brings,
    restores: &'a Restores,
    echoes: Option<&'a Echoes>,
    peers: &'a Peers,
    /// Who signs for what it takes, once it can.
    custodian: Option<&'a dyn Custodian>,
}

impl Feed<'_> {
    /// Telemetry of store `store` of `machine`: its view, changed by `f` on a copy swapped in ([`Peers`]).
    fn note(&self, machine: &str, store: &str, mut f: impl FnMut(&mut PeerView)) {
        let name = peer_name(machine, store);
        let known = self.peers.load().get(&name).cloned();
        let view = known.unwrap_or_else(|| {
            let made = Arc::new(crate::rcu::Rcu::new(PeerView { machine: machine.to_owned(), store: store.to_owned(), ..PeerView::default() }));
            self.peers.update(|all| Arc::clone(all.entry(name.clone()).or_insert_with(|| Arc::clone(&made))))
        });
        view.update(&mut f);
    }
}

/// One message from a runner, taken: what to answer.
fn take(feed: &Feed<'_>, taking: &mut HashMap<(hyper_transport::PeerId, String), Taking>, peer: hyper_transport::PeerId, machine: &str, msg: Msg, data: Vec<u8>) -> Vec<Msg> {
    let Feed { store, brings, restores, echoes, .. } = *feed;
    // Where the server's copy of a runner's store stands. When that can't be read (the store is
    // busy or failing), no answer: never "from 0", which would have the runner send everything
    // again, and every runner at once. The runner asks again after a while ([`STALL`]).
    let stands = |s: &str| match store.peer(&peer_name(machine, s)) {
        Ok(rev) => {
            feed.note(machine, s, |v| v.rev = rev);
            Some(rev)
        }
        Err(e) => {
            tracing::warn!(%machine, store = %s, error = %format!("{e:#}"), "conversation feed: where the server's copy stands couldn't be read; the runner asks again");
            None
        }
    };
    if taking.len() > TAKING_MAX {
        taking.retain(|_, t| t.batch.is_some());
    }
    match msg {
        Msg::Hello { store: s, .. } => {
            taking.remove(&(peer, s.clone()));
            feed.note(machine, &s, |v| v.hello_at = crate::util::now_ms());
            let Some(rev) = stands(&s) else { return Vec::new() };
            tracing::info!(%machine, store = %s, from = rev, "conversation feed: a runner's store sends the server's what it has after this");
            taking.entry((peer, s.clone())).or_default().told = Some(rev);
            vec![Msg::From { store: s, rev, zip: true, kept: true, about: None }]
        }
        Msg::Batch { store: s, from, upto, part, parts, zip, told, hand_on } => {
            let key = (peer, s.clone());
            // Where to go on from, about this batch: to a runner from before (it doesn't say
            // where it was told to go on from), once until a batch of it is taken. It goes on from
            // every answer, and the batches it sent before it heard one don't follow either: each
            // answered, it sent the same batches again, a storm that took nothing for minutes on a
            // busy server. A runner that says leaves out an answer about what it has sent since.
            let tell = |taking: &mut HashMap<(hyper_transport::PeerId, String), Taking>, rev: Option<u64>| -> Vec<Msg> {
                let Some(rev) = rev else { return Vec::new() };
                let t = taking.entry(key.clone()).or_default();
                if told.is_none() && t.told == Some(rev) {
                    return Vec::new();
                }
                t.told = Some(rev);
                vec![Msg::From { store: s.clone(), rev, zip: true, kept: true, about: Some(from) }]
            };
            // Where it stands, when a batch didn't come whole: what it told a runner from before,
            // if it did since a batch was last taken (nothing taken since, it still stands there).
            let stands_now = |taking: &HashMap<(hyper_transport::PeerId, String), Taking>| taking.get(&key).and_then(|t| t.told).filter(|_| told.is_none()).or_else(|| stands(&s));
            let fits = match taking.get(&key).and_then(|t| t.batch.as_ref()) {
                None => part == 0,
                Some(p) => p.from == from && p.upto == upto && p.parts == parts && p.got == part && p.zip == zip,
            };
            if !fits {
                // A part out of place (one went astray): start over from what the server has.
                if let Some(t) = taking.get_mut(&key) {
                    t.batch = None;
                }
                let rev = stands_now(taking);
                return tell(taking, rev);
            }
            let t = taking.entry(key.clone()).or_default();
            let p = t.batch.get_or_insert_with(|| Partial { from, upto, parts, got: 0, zip, bytes: Vec::new(), hand_on });
            p.got += 1;
            p.bytes.extend_from_slice(&data);
            if p.bytes.len() > BATCH_MAX {
                t.batch = None;
                tracing::warn!(%machine, "conversation feed: a batch too large to take");
                return Vec::new();
            }
            if p.got < p.parts {
                return Vec::new();
            }
            let Some(done) = t.batch.take() else { return Vec::new() };
            let wire = done.bytes.len() as u64;
            // What came, checked against its giver's hand-on before anything of it is taken
            // (docs/ci.md W27-R08): a batch that isn't what its giver signed for is refused, and
            // the receipt of what came records the break. Unzipped, checked and parsed on the
            // compute pool, this thread waiting for it ([`decode_batch`]).
            let signer = feed.custodian.zip(done.hand_on.as_deref());
            let (batch, signed) = match decode_batch(done.bytes, done.zip, signer.map(|(_, h)| handed(h))) {
                Decoded::Read(batch, sha256) => (batch, signer.zip(sha256).map(|((c, h), sha256)| (c, h.to_owned(), batch_thing(&s, from, upto, &sha256)))),
                Decoded::NotAsHanded(sha256) => {
                    tracing::warn!(%machine, store = %s, from, upto, "conversation feed: a batch isn't what its giver's hand-on says: refused");
                    if let Some((c, h)) = signer {
                        c.received(machine, h, batch_thing(&s, from, upto, &sha256));
                    }
                    feed.note(machine, &s, |v| v.errors += 1);
                    let rev = stands_now(taking);
                    return tell(taking, rev);
                }
                Decoded::Unreadable(e) => {
                    tracing::warn!(%machine, error = %format!("{e:#}"), "conversation feed: an unreadable batch");
                    feed.note(machine, &s, |v| v.errors += 1);
                    let rev = stands_now(taking);
                    return tell(taking, rev);
                }
            };
            let theirs = (brings == Brings::TheirMachine).then_some(machine);
            let copy = restores.load().contains(&s);
            let began = Instant::now();
            let took = if copy { store.take_copy(&peer_name(machine, &s), from, batch) } else { store.take(&peer_name(machine, &s), theirs, from, batch) };
            let take_ms = u64::try_from(began.elapsed().as_millis()).unwrap_or(u64::MAX);
            if copy
                && let (Ok(t), Some(echoes)) = (&took, echoes)
                && let Some((first, last)) = t.wrote
            {
                echoes.add(machine, first, last);
            }
            match took {
                Ok(t) if t.gap => {
                    feed.note(machine, &s, |v| (v.gaps, v.rev) = (v.gaps + 1, t.rev));
                    tell(taking, Some(t.rev))
                }
                Ok(t) => {
                    if t.fenced > 0 {
                        tracing::info!(%machine, fenced = t.fenced, "conversation feed: rows of sessions that went on elsewhere were left out");
                    }
                    feed.note(machine, &s, |v| {
                        v.batches += 1;
                        v.rows += t.applied as u64;
                        v.fenced += t.fenced as u64;
                        v.left += t.left as u64;
                        v.same += t.same as u64;
                        v.bytes += wire;
                        (v.rev, v.last_batch, v.take_ms, v.copy) = (t.rev, crate::util::now_ms(), take_ms, copy);
                    });
                    // Taken: whatever comes next that doesn't follow is told where to go on.
                    taking.remove(&key);
                    if let Some((c, h, got)) = signed {
                        c.received(machine, &h, got);
                    }
                    vec![Msg::Ack { store: s, rev: t.rev }]
                }
                Err(e) => {
                    // Not taken: the runner asks again after a while, and sends it again.
                    tracing::warn!(%machine, error = %format!("{e:#}"), "conversation feed: a batch couldn't be taken");
                    feed.note(machine, &s, |v| v.errors += 1);
                    Vec::new()
                }
            }
        }
        Msg::From { .. } | Msg::Ack { .. } | Msg::Restore { .. } | Msg::Restored { .. } | Msg::Stats { .. } => Vec::new(),
    }
}

// ----------------------------------------------------------------------- a whole batch, decoded

/// What a whole batch's bytes came to ([`decode_batch`]).
enum Decoded {
    /// Its rows, with the SHA-256 of their JSON where it was checked.
    Read(Changes, Option<String>),
    /// Its JSON isn't what its giver's hand-on says (this is its SHA-256): refused before it's parsed.
    NotAsHanded(String),
    /// It couldn't be unzipped or parsed.
    Unreadable(anyhow::Error),
}

/// A whole batch's `bytes` (brotli-compressed when `zip`) unzipped, checked and parsed on the compute pool, in
/// pieces (goal 3, plan 01a1a59c2a8e step 3): unzipped and hashed a buffer at a time and parsed a row at a
/// time, the piece given up where another job waits for its thread ([`Decoding`]). The intake's thread did
/// all three at once, a batch of two MiB at a time, at the shards' own priority. `check`, where the batch is
/// checked against its giver's hand-on: the SHA-256 the hand-on says it has (none where it says none that
/// can be read); checked before the batch is parsed. The intake's thread waits for it, so the batches it
/// takes are taken in the order they came.
fn decode_batch(bytes: Vec<u8>, zip: bool, check: Option<Option<String>>) -> Decoded {
    crate::rt::compute_pieces_blocking(Decoding::new(bytes, zip, check), |d, turn| d.piece(turn)).unwrap_or_else(Decoded::Unreadable)
}

/// A whole batch being decoded ([`decode_batch`]): where it stands.
struct Decoding {
    /// Its JSON so far: as it came, or as far as it's unzipped.
    json: Vec<u8>,
    /// What's left to unzip, when it came compressed.
    unzip: Option<brotli::Decompressor<std::io::Cursor<Vec<u8>>>>,
    /// Where it's checked: its SHA-256 so far, how much of `json` that has taken, and what its hand-on says.
    check: Option<(sha2::Sha256, usize, Option<String>)>,
    /// Its JSON's SHA-256, once checked.
    sha256: Option<String>,
    /// Its rows, as far as they're read.
    reading: Reading,
}

impl Decoding {
    fn new(bytes: Vec<u8>, zip: bool, check: Option<Option<String>>) -> Self {
        use sha2::Digest as _;
        let (json, unzip) = if zip { (Vec::new(), Some(brotli::Decompressor::new(std::io::Cursor::new(bytes), ZIP_BUFFER))) } else { (bytes, None) };
        Self { json, unzip, check: check.map(|says| (sha2::Sha256::new(), 0, says)), sha256: None, reading: Reading::default() }
    }

    /// A piece: units of the work until it's done, or until its turn says another job waits for the thread.
    fn piece(&mut self, turn: &crate::rt::Turn<'_>) -> crate::rt::Step<Decoded> {
        loop {
            match self.unit() {
                Ok(Some(done)) => return crate::rt::Step::Done(done),
                Ok(None) if turn.over() => return crate::rt::Step::More,
                Ok(None) => {}
                Err(e) => return crate::rt::Step::Done(Decoded::Unreadable(e)),
            }
        }
    }

    /// One unit, in order: a buffer of what's come and isn't hashed yet, hashed; a buffer unzipped; the check,
    /// once all of it is in; a step of reading its rows. What it came to, once it's done.
    fn unit(&mut self) -> anyhow::Result<Option<Decoded>> {
        use sha2::Digest as _;
        use std::io::Read as _;
        if let Some((sha256, hashed, _)) = &mut self.check
            && *hashed < self.json.len()
        {
            let to = hashed.saturating_add(ZIP_BUFFER).min(self.json.len());
            sha256.update(self.json.get(*hashed..to).unwrap_or_default());
            *hashed = to;
            return Ok(None);
        }
        if let Some(unzip) = &mut self.unzip {
            let at = self.json.len();
            self.json.resize(at.saturating_add(ZIP_BUFFER), 0);
            let (n, ended) = match unzip.read(self.json.get_mut(at..).unwrap_or_default()) {
                Ok(n) => (n, n == 0),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => (0, false),
                Err(e) => return Err(e.into()),
            };
            self.json.truncate(at.saturating_add(n));
            anyhow::ensure!(self.json.len() <= BATCH_MAX, "a batch too large to take");
            if ended {
                self.unzip = None;
            }
            return Ok(None);
        }
        if let Some((sha256, _, says)) = self.check.take() {
            let sha256 = hex::encode(sha256.finalize());
            if says.as_deref() != Some(sha256.as_str()) {
                return Ok(Some(Decoded::NotAsHanded(sha256)));
            }
            self.sha256 = Some(sha256);
            return Ok(None);
        }
        Ok(self.reading.step(&self.json)?.map(|batch| Decoded::Read(batch, self.sha256.take())))
    }
}

/// A batch's [`Changes`] read from its JSON a step at a time ([`Decoding`]): a member's name and value, or
/// one of its rows. Where its JSON isn't as a feed writes it (a member missing or repeated, rows that aren't
/// an array, anything after the object), it's read whole instead, as it was before, so what's read, or why
/// it can't be, is serde's own.
#[derive(Default)]
struct Reading {
    /// Where the next thing starts in the JSON.
    at: usize,
    within: Within,
    rows: Option<Vec<Change>>,
    upto: Option<u64>,
}

/// Where a [`Reading`] stands in a batch's object.
#[derive(Default, Clone, Copy, PartialEq, Eq)]
enum Within {
    /// Before the object.
    #[default]
    Start,
    /// Before a member's name.
    Name,
    /// Before one of the rows.
    Row,
    /// After a member's value: a comma, or the object's end.
    After,
}

impl Reading {
    /// A step: the batch, once it's all read.
    fn step(&mut self, json: &[u8]) -> anyhow::Result<Option<Changes>> {
        match self.as_written(json) {
            Some(read) => Ok(read),
            None => Ok(Some(serde_json::from_slice(json)?)),
        }
    }

    /// A step of a batch as a feed writes it: none where the JSON isn't.
    fn as_written(&mut self, json: &[u8]) -> Option<Option<Changes>> {
        self.at = past_space(json, self.at);
        match self.within {
            Within::Start => {
                self.at = past(json, self.at, b'{')?;
                self.within = Within::Name;
            }
            Within::Name => {
                let name: String = self.value(json)?;
                self.at = past(json, past_space(json, self.at), b':')?;
                self.at = past_space(json, self.at);
                match name.as_str() {
                    "rows" if self.rows.is_none() => {
                        self.at = past_space(json, past(json, self.at, b'[')?);
                        self.rows = Some(Vec::new());
                        self.within = match past(json, self.at, b']') {
                            Some(at) => {
                                self.at = at;
                                Within::After
                            }
                            None => Within::Row,
                        };
                    }
                    "upto" if self.upto.is_none() => {
                        self.upto = Some(self.value(json)?);
                        self.within = Within::After;
                    }
                    "rows" | "upto" => return None,
                    _ => {
                        let _: serde::de::IgnoredAny = self.value(json)?;
                        self.within = Within::After;
                    }
                }
            }
            Within::Row => {
                let row: Change = self.value(json)?;
                self.rows.as_mut()?.push(row);
                let at = past_space(json, self.at);
                match json.get(at) {
                    Some(b',') => self.at = at + 1,
                    Some(b']') => {
                        self.at = at + 1;
                        self.within = Within::After;
                    }
                    _ => return None,
                }
            }
            Within::After => match json.get(self.at) {
                Some(b',') => {
                    self.at += 1;
                    self.within = Within::Name;
                }
                Some(b'}') if past_space(json, self.at + 1) == json.len() => return Some(Some(Changes { rows: self.rows.take()?, upto: self.upto.take()? })),
                _ => return None,
            },
        }
        Some(None)
    }

    /// The JSON value where the reading stands, as a `T`, and the reading moved past it.
    fn value<'a, T: serde::Deserialize<'a>>(&mut self, json: &'a [u8]) -> Option<T> {
        let mut values = serde_json::Deserializer::from_slice(json.get(self.at..)?).into_iter::<T>();
        let value = values.next()?.ok()?;
        self.at = self.at.checked_add(values.byte_offset())?;
        Some(value)
    }
}

/// Past the JSON whitespace at `at`.
fn past_space(json: &[u8], at: usize) -> usize {
    at + json.get(at..).map_or(0, |rest| rest.iter().take_while(|b| matches!(b, b' ' | b'\n' | b'\r' | b'\t')).count())
}

/// Past `byte`, where it's at `at`.
fn past(json: &[u8], at: usize, byte: u8) -> Option<usize> {
    (json.get(at) == Some(&byte)).then_some(at + 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::core::Update;
    use crate::harness::model::{Body, Event, State};

    fn said(id: &str, seq: u64, text: &str) -> Update {
        Update::Events { events: vec![Event { id: id.into(), seq, at: 1_000 + seq as i64, parent: None, by: None, body: Body::Assistant { text: text.into(), streaming: false } }] }
    }

    /// Everything `runner` has, sent to `server` the way the feed does it (batches of `bytes`,
    /// each in parts), and the number the server has at the end.
    /// [`take`] with telemetry of its own, thrown away.
    #[allow(clippy::too_many_arguments)]
    fn took(store: &Store, partial: &mut HashMap<(hyper_transport::PeerId, String), Taking>, peer: hyper_transport::PeerId, machine: &str, brings: Brings, json: &str, data: Vec<u8>, restores: &Restores, echoes: Option<&Echoes>) -> Vec<Msg> {
        let peers = Peers::default();
        let Some(msg) = decode(json) else { return Vec::new() };
        take(&Feed { store, brings, restores, echoes, peers: &peers, custodian: None }, partial, peer, machine, msg, data)
    }

    fn feed_all(runner: &Store, server: &Store, machine: &str, bytes: usize) -> anyhow::Result<u64> {
        feed_as(runner, server, machine, Brings::TheirMachine, bytes)
    }

    fn feed_as(runner: &Store, server: &Store, machine: &str, brings: Brings, bytes: usize) -> anyhow::Result<u64> {
        let mut partial = HashMap::new();
        let hello = took(server, &mut partial, 7, machine, brings, &encode(&Msg::Hello { store: runner.id().to_owned(), restore: false }), Vec::new(), &Restores::default(), None);
        let Some(Msg::From { mut rev, zip, kept, .. }) = hello.into_iter().next() else { anyhow::bail!("no answer to hello") };
        let told = Some(rev);
        loop {
            let batch = runner.changes_as(rev, bytes, kept)?;
            if batch.rows.is_empty() {
                return Ok(rev);
            }
            let mut answer = Vec::new();
            for (m, data) in parts(runner.id(), rev, &batch, zip, told)? {
                answer = took(server, &mut partial, 7, machine, brings, &encode(&m), data, &Restores::default(), None);
            }
            match answer.first() {
                Some(Msg::Ack { rev: r, .. }) => rev = *r,
                other => anyhow::bail!("the server answered {other:?}"),
            }
        }
    }

    fn line(session: &str, source: &str, n: u64, text: &str) -> super::super::rows::Row {
        super::super::rows::Row::Record(Box::new(super::super::rows::RecordRow { session: session.into(), source: source.into(), n, end: (n + 1) * 100, line: text.into(), kept: None }))
    }

    #[test]
    fn a_runner_store_reaches_the_server_whole_in_order_and_goes_on_from_where_it_was() -> anyhow::Result<()> {
        let (_r, runner) = super::super::store::scratch()?;
        let (_s, server) = super::super::store::scratch()?;
        runner.send("a1", &Update::State { state: Box::new(State { title: "parser".into(), ..State::default() }) });
        for i in 1..=300u64 {
            runner.send("a1", &said(&format!("m{i}"), i, &format!("reply {i} {}", "x".repeat(2_000))));
        }
        // A line larger than a part: it goes in parts and comes back whole.
        let big = format!("{{\"type\":\"user\",\"blob\":\"{}\"}}", "y".repeat(PART + 1234));
        runner.put(vec![line("a1", "s-1", 0, "{\"type\":\"summary\"}"), line("a1", "s-1", 1, &big)]);
        runner.sync()?;
        let first = feed_all(&runner, &server, "box", 64 << 10)?;
        assert_eq!(server.kept("a1", 1_000)?.map(|k| k.events.len()), Some(300));
        assert_eq!(server.record("a1", "s-1", 0, 10)?.into_iter().map(|(_, l)| l.len()).collect::<Vec<_>>(), vec!["{\"type\":\"summary\"}".len(), big.len()]);
        // More, a rewind and a file that starts over: only what came after goes, and the server
        // ends up with what the runner has.
        runner.send("a1", &said("m5", 5, "five, revised"));
        runner.send("a1", &Update::Reset { from: Some(290) });
        for i in 290..=295u64 {
            runner.send("a1", &said(&format!("m{i}"), i, "again"));
        }
        runner.put(vec![super::super::rows::Row::RecordReset { session: "a1".into(), source: "s-1".into(), at: 5 }, line("a1", "s-1", 0, "{\"type\":\"fresh\"}")]);
        runner.sync()?;
        let second = feed_all(&runner, &server, "box", 64 << 10)?;
        assert!(second > first);
        let ids = |s: &Store| -> anyhow::Result<Vec<(String, String)>> { Ok(s.kept("a1", 1_000)?.map(|k| k.events.into_iter().map(|e| (e.id, e.text)).collect()).unwrap_or_default()) };
        assert_eq!(ids(&server)?, ids(&runner)?);
        assert_eq!(ids(&server)?.len(), 295);
        assert_eq!(server.record("a1", "s-1", 0, 10)?, vec![(0, "{\"type\":\"fresh\"}".to_owned())]);
        // Nothing new: nothing sent, the number stays.
        assert_eq!(feed_all(&runner, &server, "box", 64 << 10)?, second);
        Ok(())
    }

    #[test]
    fn a_mirror_keeps_the_server_store_and_where_each_session_runs() -> anyhow::Result<()> {
        let (_a, a) = super::super::store::scratch()?;
        let (_s, server) = super::super::store::scratch()?;
        let (_m, mirror) = super::super::store::scratch()?;
        let info = serde_json::json!({ "id": "s1", "kind": "claude", "kind_name": "Claude Code", "target": "", "name": "parser", "cwd": "/w", "created_at": 1, "status": "idle", "title": "", "model": "", "mode": "", "approvals": [], "queued": 0, "tasks_running": 0, "cost_usd": 0.0, "last_activity": 1, "goal": "" }).to_string();
        a.put(vec![super::super::rows::Row::Session { session: "s1".into(), info: Some(info), meta: None, state: None, changes: None, epoch: None, open: true, at: crate::util::now_ms() }]);
        a.send("s1", &said("m1", 1, "kept twice"));
        a.put(vec![line("s1", "cc-1", 0, "{\"type\":\"user\"}")]);
        a.sync()?;
        feed_all(&a, &server, "box-a", 1 << 20)?;
        // The server's store to its mirror: its rows say where each session runs.
        feed_as(&server, &mirror, "server", Brings::AsItSays, 1 << 20)?;
        let kept = mirror.unended(0)?;
        assert_eq!(kept.iter().map(|u| (u.id.as_str(), u.machine.as_deref())).collect::<Vec<_>>(), vec![("s1", Some("box-a"))]);
        assert_eq!(mirror.kept("s1", 10)?.map(|k| k.events.len()), Some(1));
        assert_eq!(mirror.record("s1", "cc-1", 0, 10)?, server.record("s1", "cc-1", 0, 10)?);
        // More on the runner reaches the mirror through the server, from where each left off.
        a.send("s1", &said("m2", 2, "and again"));
        a.sync()?;
        feed_all(&a, &server, "box-a", 1 << 20)?;
        feed_as(&server, &mirror, "server", Brings::AsItSays, 1 << 20)?;
        assert_eq!(mirror.kept("s1", 10)?.map(|k| k.events.into_iter().map(|e| e.id).collect::<Vec<_>>()), Some(vec!["m1".to_owned(), "m2".to_owned()]));
        Ok(())
    }

    #[test]
    fn a_batch_that_does_not_follow_what_the_server_has_is_refused_with_where_it_stands() -> anyhow::Result<()> {
        let (_r, runner) = super::super::store::scratch()?;
        let (_s, server) = super::super::store::scratch()?;
        for i in 1..=3u64 {
            runner.send("a1", &said(&format!("m{i}"), i, "x"));
        }
        runner.sync()?;
        let batch = runner.changes(2, 1 << 20)?;
        let from = |rev: u64, about: u64| vec![Msg::From { store: runner.id().to_owned(), rev, zip: true, kept: true, about: Some(about) }];
        let send = |partial: &mut HashMap<(hyper_transport::PeerId, String), Taking>, peer: hyper_transport::PeerId, msgs: Vec<(Msg, Vec<u8>)>| {
            let mut answer = Vec::new();
            for (m, data) in msgs {
                answer = took(&server, partial, peer, "box", Brings::TheirMachine, &encode(&m), data, &Restores::default(), None);
            }
            answer
        };
        let mut partial = HashMap::new();
        // A runner that says where it was told to go on from is answered each time.
        assert_eq!(send(&mut partial, 7, parts(runner.id(), 2, &batch, true, Some(0))?), from(0, 2));
        assert_eq!(send(&mut partial, 7, parts(runner.id(), 2, &batch, true, Some(0))?), from(0, 2));
        assert_eq!(server.kept("a1", 10)?, None);
        // A part out of place: the same.
        let (m, data) = parts(runner.id(), 0, &runner.changes(0, 1 << 20)?, true, Some(0))?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no part"))?;
        let Msg::Batch { store, from: f, upto, parts: n, .. } = m else { anyhow::bail!("not a batch") };
        let astray = |told| (Msg::Batch { store: store.clone(), from: f, upto, part: 1, parts: n + 1, zip: true, told, hand_on: None }, data.clone());
        assert_eq!(send(&mut partial, 7, vec![astray(Some(0))]), from(0, 0));
        // A runner from before goes on from every answer, and what it sent before it heard one
        // doesn't follow either: told once, until a batch of it is taken.
        assert_eq!(send(&mut partial, 8, parts(runner.id(), 2, &batch, true, None)?), from(0, 2));
        assert_eq!(send(&mut partial, 8, parts(runner.id(), 2, &batch, true, None)?), Vec::new());
        assert_eq!(send(&mut partial, 8, vec![astray(None)]), Vec::new());
        let whole = runner.changes(0, 1 << 20)?;
        assert!(matches!(send(&mut partial, 8, parts(runner.id(), 0, &whole, true, None)?).first(), Some(Msg::Ack { rev: 3, .. })));
        assert_eq!(send(&mut partial, 8, parts(runner.id(), 2, &batch, true, None)?), from(3, 2));
        Ok(())
    }

    #[test]
    fn a_server_that_cannot_read_where_it_stands_says_nothing_rather_than_ask_for_everything() {
        let server = super::super::store::stopped();
        let mut partial = HashMap::new();
        let restores = Restores::default();
        let mut answer = |m: Msg, data: &[u8]| took(&server, &mut partial, 7, "box", Brings::TheirMachine, &encode(&m), data.to_vec(), &restores, None);
        assert_eq!(answer(Msg::Hello { store: "r".into(), restore: false }, b""), Vec::new());
        assert_eq!(answer(Msg::Hello { store: "r".into(), restore: true }, b""), Vec::new());
        // A part out of place, and a batch that can't be read: the same.
        assert_eq!(answer(Msg::Batch { store: "r".into(), from: 0, upto: 5, part: 1, parts: 2, zip: false, told: Some(0), hand_on: None }, b"{}"), Vec::new());
        assert_eq!(answer(Msg::Batch { store: "r".into(), from: 0, upto: 5, part: 0, parts: 1, zip: false, told: Some(0), hand_on: None }, b"not json"), Vec::new());
    }

    #[test]
    fn a_session_that_went_on_elsewhere_takes_nothing_more_from_the_machine_it_left() -> anyhow::Result<()> {
        let (_a, a) = super::super::store::scratch()?;
        let (_b, b) = super::super::store::scratch()?;
        let (_s, server) = super::super::store::scratch()?;
        a.send("s1", &Update::State { state: Box::new(State { title: "on a".into(), ..State::default() }) });
        a.send("s1", &said("m1", 1, "said on a"));
        a.sync()?;
        feed_all(&a, &server, "a", 1 << 20)?;
        // It moves to b (epoch 1); b says so, then goes on.
        b.send("s1", &Update::State { state: Box::new(State { title: "on b".into(), epoch: 1, ..State::default() }) });
        b.send("s1", &said("m1", 1, "said on a"));
        b.send("s1", &said("m2", 2, "said on b"));
        b.sync()?;
        feed_all(&b, &server, "b", 1 << 20)?;
        // a, late, still writes to its copy (it came back after the move): none of it counts.
        a.send("s1", &said("m2", 2, "stale, from a"));
        a.send("s1", &Update::State { state: Box::new(State { title: "stale".into(), ..State::default() }) });
        a.sync()?;
        feed_all(&a, &server, "a", 1 << 20)?;
        let k = server.kept("s1", 10)?.ok_or_else(|| anyhow::anyhow!("nothing kept"))?;
        assert_eq!(k.events.iter().map(|e| e.text.as_str()).collect::<Vec<_>>(), vec!["said on a", "said on b"]);
        assert!(k.state.as_deref().is_some_and(|s| s.contains("on b")));
        Ok(())
    }

    /// A mirror's copy sent back restores what the server's store lost: a session it has nothing
    /// of comes back where it ran, one it has from elsewhere stays as it is, and the copy ends
    /// with what it held when it began.
    #[test]
    fn a_copy_sent_back_restores_what_the_server_lost() -> anyhow::Result<()> {
        use super::super::rows::Row;
        crate::rt::block_on(async move {
            let (_m, mirror) = super::super::store::scratch()?;
            let (_s, server) = super::super::store::scratch()?;
            // The mirror's copy: a session of a machine that is gone, and one still running.
            let session = |s: &str| Row::Session { session: s.into(), info: Some("{}".into()), meta: None, state: None, changes: None, epoch: None, open: true, at: 5 };
            mirror.put(vec![session("gone"), Row::Machine { session: "gone".into(), machine: "old-box".into() }, session("live")]);
            mirror.send("gone", &said("g1", 1, "from a machine that is gone"));
            mirror.send("live", &said("l1", 1, "as it was"));
            mirror.sync()?;
            // The server's store, made again: the running session's machine sent it already.
            server.put(vec![session("live")]);
            server.send("live", &said("l1", 1, "as it is now"));
            server.sync()?;
            let restores = Restores::default();
            restores.update(|all| {
                all.insert(mirror.id().to_owned());
            });
            let (ship, out) = Shipper::start_restore(mirror.clone());
            ship.link(Link::Up);
            let mut partial = HashMap::new();
            let mut restored = None;
            for _ in 0..50 {
                let Ok(Ok((json, data))) = crate::rt::timeout(Duration::from_secs(5), out.recv()).await else { break };
                if let Some(Msg::Restored { store, rev }) = decode(&json) {
                    restored = Some((store, rev));
                    break;
                }
                for m in took(&server, &mut partial, 7, "mirror-box", Brings::AsItSays, &json, data, &restores, None) {
                    ship.link(Link::Heard(m));
                }
            }
            assert_eq!(restored, Some((mirror.id().to_owned(), mirror.top()?)));
            assert_eq!(server.kept("gone", 10)?.map(|k| k.events.len()), Some(1));
            assert_eq!(server.unended(0)?.into_iter().find(|u| u.id == "gone").and_then(|u| u.machine).as_deref(), Some("old-box"));
            assert_eq!(server.kept("live", 10)?.and_then(|k| k.events.first().map(|e| e.text.clone())).as_deref(), Some("as it is now"));
            assert_eq!(server.end_copy()?, 1);
            Ok(())
        })
    }

    #[test]
    fn the_shipper_sends_what_the_store_writes_and_starts_over_after_a_gap() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (_r, runner) = super::super::store::scratch()?;
            let (_s, server) = super::super::store::scratch()?;
            let (ship, out) = Shipper::start(runner.clone());
            ship.link(Link::Up);
            let mut partial = HashMap::new();
            // The server's side, in this test: what the shipper sends is taken, answers go back.
            let answer = |json: String, data: Vec<u8>, partial: &mut HashMap<(hyper_transport::PeerId, String), Taking>| {
                for m in took(&server, partial, 7, "box", Brings::TheirMachine, &json, data, &Restores::default(), None) {
                    ship.link(Link::Heard(m));
                }
            };
            let (json, data) = crate::rt::timeout(Duration::from_secs(5), out.recv()).await??;
            assert!(matches!(decode(&json), Some(Msg::Hello { .. })));
            answer(json, data, &mut partial);
            runner.send("a1", &said("m1", 1, "hello"));
            for _ in 0..50 {
                let Ok(Ok((json, data))) = crate::rt::timeout(Duration::from_secs(5), out.recv()).await else { break };
                answer(json, data, &mut partial);
                if server.kept("a1", 10)?.is_some() {
                    break;
                }
            }
            assert_eq!(server.kept("a1", 10)?.map(|k| k.events.len()), Some(1));
            Ok(())
        })
    }

    /// Goal 4, plan 01a1a59c2a9c step 5: a feed's queues, lock-free now (`crate::memory::queue`). Its link's
    /// words, sent from every shard at once, reach it each sender's in order, none refused, lost or doubled
    /// (unbounded, as before). Its parts to send hold [`OUT_PARTS`] whatever each carries and hold the feed
    /// back past them, in order. An intake's answers hold [`ANSWERS`], its thread waiting past them.
    #[test]
    fn g4_s5_a_feeds_queues_keep_their_order_and_their_bounds() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (tx, rx) = links();
            let (senders, per) = (8usize, 500u64);
            let mut sending = Vec::new();
            for s in 0..senders {
                let tx = tx.clone();
                sending.push(crate::rt::spawn_spread_joined(async move { (0..per).all(|rev| tx.try_send(Link::Heard(Msg::Ack { store: s.to_string(), rev })).is_ok()) }));
            }
            drop(tx);
            for s in sending {
                anyhow::ensure!(s.await.unwrap_or(false), "a link's word was refused");
            }
            let mut got = vec![Vec::new(); senders];
            while let Ok(l) = rx.try_recv() {
                let Link::Heard(Msg::Ack { store, rev }) = l else { anyhow::bail!("a word no sender of the test's sent") };
                got.get_mut(store.parse::<usize>()?).ok_or_else(|| anyhow::anyhow!("no sender {store}"))?.push(rev);
            }
            for (s, theirs) in got.iter().enumerate() {
                anyhow::ensure!(theirs.iter().copied().eq(0..per), "sender {s}'s: {} of {per}, in order: {}", theirs.len(), theirs.windows(2).all(|w| w.first() < w.get(1)));
            }
            // Parts: as many as their places, a whole part or none; the next waits until one is taken.
            let (out, parts) = parts_out();
            for n in 0..OUT_PARTS {
                anyhow::ensure!(out.try_send((n.to_string(), vec![0; if n == 0 { PART } else { 0 }])).is_ok(), "part {n} had no room");
            }
            anyhow::ensure!(matches!(out.try_send(("over".into(), Vec::new())), Err(async_channel::TrySendError::Full(_))), "a part past its places");
            let waiting = crate::rt::spawn_spread_joined({
                let out = out.clone();
                async move { out.send(("last".into(), Vec::new())).await.is_ok() }
            });
            crate::rt::sleep(Duration::from_millis(50)).await;
            anyhow::ensure!(parts.len() == OUT_PARTS, "the feed didn't wait for room: {} parts", parts.len());
            for n in 0..OUT_PARTS {
                let (json, _) = crate::rt::timeout(Duration::from_secs(5), parts.recv()).await??;
                anyhow::ensure!(json == n.to_string(), "part {n} came as {json}");
            }
            let (json, _) = crate::rt::timeout(Duration::from_secs(5), parts.recv()).await??;
            anyhow::ensure!(json == "last" && waiting.await.unwrap_or(false), "the waiting part, last: {json}");
            // Answers: as many as their places; the intake's thread waits past them.
            let (answer, told) = answers();
            for n in 0..ANSWERS {
                anyhow::ensure!(answer.try_send(Out { peer: 7, json: n.to_string() }).is_ok(), "answer {n} had no room");
            }
            anyhow::ensure!(matches!(answer.try_send(Out { peer: 7, json: "over".into() }), Err(async_channel::TrySendError::Full(_))), "an answer past its places");
            let thread = std::thread::spawn(move || answer.send_blocking(Out { peer: 7, json: "last".into() }).is_ok());
            for n in 0..ANSWERS {
                let o = crate::rt::timeout(Duration::from_secs(5), told.recv()).await??;
                anyhow::ensure!(o.json == n.to_string() && o.peer == 7, "answer {n} came as {}", o.json);
            }
            let last = crate::rt::timeout(Duration::from_secs(5), told.recv()).await??;
            anyhow::ensure!(last.json == "last" && thread.join().unwrap_or(false), "the waiting answer, last: {}", last.json);
            Ok(())
        })
    }

    /// What the shipper sends next, within `wait` (None: nothing).
    async fn next_sent(out: &crate::memory::queue::Receiver<(String, Vec<u8>)>, wait: Duration) -> Option<Msg> {
        crate::rt::timeout(wait, out.recv()).await.ok().and_then(Result::ok).and_then(|(json, _)| decode(&json))
    }

    /// A test's end of a feed: as a giver, a hand-on of each batch saying what the giver's would (its
    /// digest); as a taker, each receipt noted.
    #[derive(Default)]
    struct Sides {
        received: parking_lot::Mutex<Vec<(String, Subject)>>,
    }

    impl Custodian for Sides {
        fn hand_on(&self, thing: Subject) -> BoxFuture<'_, Option<String>> {
            Box::pin(async move { Some(bundle_of(&thing)) })
        }

        fn received(&self, _machine: &str, hand_on: &str, thing: Subject) {
            self.received.lock().push((hand_on.to_owned(), thing));
        }
    }

    /// A hand-on of `thing` as a bundle's JSON, unsigned: what [`handed`] reads of one.
    fn bundle_of(thing: &Subject) -> String {
        use base64::Engine as _;
        use kiln_ci::identity::statement::{PAYLOAD_CUSTODY, Statement, payload};
        let st = Statement { subject: vec![thing.clone()], ..Statement::new(serde_json::json!({ "v": 1, "kind": "handed-on", "by": "kiln://hyper-light/person/ada/machine/box/instance/i", "to": "kiln://hyper-light/server" })) };
        let encoded = base64::engine::general_purpose::STANDARD.encode(payload(&st).unwrap_or_default());
        serde_json::json!({ "dsseEnvelope": { "payloadType": PAYLOAD_CUSTODY, "payload": encoded, "signatures": [] } }).to_string()
    }

    /// W27-R08 (the Transfers plan's step 12): a batch comes with its giver's hand-on, by the
    /// SHA-256 of its rows, and the taker checks what came against it before its store takes any
    /// of it. One that isn't what its hand-on says is refused (the giver told where to go on from)
    /// and the receipt of what came records the break; as it was signed for, it's taken and
    /// received; one with no hand-on (a giver from before) is taken as before, with no receipt.
    #[test]
    fn w27_r08_a_feeds_batch_is_taken_only_as_its_giver_signed_for_it() -> anyhow::Result<()> {
        let (_r, runner) = super::super::store::scratch()?;
        let (_s, server) = super::super::store::scratch()?;
        runner.send("a1", &said("m1", 1, "one"));
        runner.sync()?;
        let signs = Sides::default();
        let (restores, peers) = (Restores::default(), Peers::default());
        let feed = Feed { store: &server, brings: Brings::TheirMachine, restores: &restores, echoes: None, peers: &peers, custodian: Some(&signs) };
        let mut taking = HashMap::new();
        let hello = take(&feed, &mut taking, 7, "box", Msg::Hello { store: runner.id().to_owned(), restore: false }, Vec::new());
        let Some(Msg::From { rev, zip, kept, .. }) = hello.into_iter().next() else { anyhow::bail!("no answer to hello") };
        let batch = runner.changes_as(rev, BATCH_BYTES, kept)?;
        let json = serde_json::to_vec(&batch)?;
        let thing = batch_thing(runner.id(), rev, batch.upto, &crate::util::sha256_hex(&json));
        let send = |taking: &mut HashMap<(hyper_transport::PeerId, String), Taking>, json: Vec<u8>, hand_on: Option<String>| -> anyhow::Result<Vec<Msg>> {
            let mut answer = Vec::new();
            for (m, data) in parts_of(runner.id(), rev, batch.upto, json, zip, Some(rev), hand_on)? {
                answer = take(&feed, taking, 7, "box", m, data);
            }
            Ok(answer)
        };
        // Changed on its way: refused, nothing of it taken, and the receipt of what came.
        let mut changed = json.clone();
        if let Some(b) = changed.iter_mut().rev().find(|b| b.is_ascii_alphabetic()) {
            *b ^= 0x20;
        }
        let answer = send(&mut taking, changed.clone(), Some(bundle_of(&thing)))?;
        assert!(matches!(answer.first(), Some(Msg::From { rev: r, about: Some(_), .. }) if *r == rev), "refused, told where to go on from: {answer:?}");
        assert_eq!(server.peer(&peer_name("box", runner.id()))?, rev, "nothing taken");
        let got = signs.received.lock().pop().map(|(_, t)| t);
        assert_eq!(got.as_ref().and_then(|t| t.digest.get("sha256")), Some(&crate::util::sha256_hex(&changed)));
        // As it was signed for: taken, and received.
        let answer = send(&mut taking, json.clone(), Some(bundle_of(&thing)))?;
        assert!(matches!(answer.first(), Some(Msg::Ack { rev: r, .. }) if *r == batch.upto), "{answer:?}");
        assert_eq!(signs.received.lock().pop().map(|(_, t)| t), Some(thing));
        // A giver from before: taken as before, no receipt.
        runner.send("a1", &said("m2", 2, "two"));
        runner.sync()?;
        let next = runner.changes_as(batch.upto, BATCH_BYTES, kept)?;
        let mut answer = Vec::new();
        for (m, data) in parts(runner.id(), batch.upto, &next, zip, Some(batch.upto))? {
            answer = take(&feed, &mut taking, 7, "box", m, data);
        }
        assert!(matches!(answer.first(), Some(Msg::Ack { .. })), "{answer:?}");
        assert!(signs.received.lock().is_empty());
        Ok(())
    }

    /// W27-R08 (the Transfers plan's step 12): a shipper given a custodian sends each batch with its
    /// hand-on on its first part, by the SHA-256 of what it sends, which the taker receives.
    #[test]
    fn w27_r08_a_shipper_hands_on_each_batch_it_sends() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (_r, runner) = super::super::store::scratch()?;
            let (_s, server) = super::super::store::scratch()?;
            for i in 1..=3u64 {
                runner.send("a1", &said(&format!("m{i}"), i, &"x".repeat(1 << 20)));
            }
            runner.sync()?;
            let (ship, out) = Shipper::start(runner.clone());
            ship.sign_with(Arc::new(Sides::default()));
            ship.link(Link::Up);
            let taker = Sides::default();
            let (restores, peers) = (Restores::default(), Peers::default());
            let feed = Feed { store: &server, brings: Brings::TheirMachine, restores: &restores, echoes: None, peers: &peers, custodian: Some(&taker) };
            let mut taking = HashMap::new();
            let top = runner.top()?;
            for _ in 0..100 {
                let Ok(Ok((json, data))) = crate::rt::timeout(Duration::from_secs(5), out.recv()).await else { break };
                let Some(msg) = decode(&json) else { anyhow::bail!("an unreadable message: {json}") };
                if let Msg::Batch { part, hand_on, .. } = &msg {
                    assert_eq!(hand_on.is_some(), *part == 0, "its hand-on with its first part only");
                }
                for m in take(&feed, &mut taking, 7, "box", msg, data) {
                    ship.link(Link::Heard(m));
                }
                if server.peer(&peer_name("box", runner.id()))? >= top {
                    break;
                }
            }
            let received = taker.received.lock().clone();
            assert!(!received.is_empty(), "each batch received");
            for (h, t) in &received {
                assert_eq!(handed(h).as_ref(), t.digest.get("sha256"), "received as handed on");
            }
            Ok(())
        })
    }

    #[test]
    fn a_shipper_told_again_where_it_goes_on_from_sends_what_is_on_its_way_once() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (_r, runner) = super::super::store::scratch()?;
            runner.send("a1", &said("m1", 1, "hello"));
            runner.sync()?;
            let (ship, out) = Shipper::start(runner.clone());
            ship.link(Link::Up);
            assert!(matches!(next_sent(&out, Duration::from_secs(5)).await, Some(Msg::Hello { .. })));
            let me = runner.id().to_owned();
            let from = |about| Link::Heard(Msg::From { store: me.clone(), rev: 0, zip: false, kept: false, about });
            ship.link(from(None));
            assert!(matches!(next_sent(&out, Duration::from_secs(5)).await, Some(Msg::Batch { from: 0, told: Some(0), .. })));
            // Told again while that batch is on its way: about one sent before it went on from 0
            // (it didn't follow either), and a hello's answer come late. The batch goes once.
            ship.link(from(Some(7)));
            ship.link(from(None));
            assert_eq!(next_sent(&out, Duration::from_millis(1_500)).await, None);
            // About the batch it sent since: that one was lost on the way, and goes again.
            ship.link(from(Some(0)));
            assert!(matches!(next_sent(&out, Duration::from_secs(5)).await, Some(Msg::Batch { from: 0, told: Some(0), .. })));
            Ok(())
        })
    }

    #[test]
    fn a_taker_that_takes_lines_as_kept_gets_them_compressed_and_one_from_before_gets_text() -> anyhow::Result<()> {
        let (_r, runner) = super::super::store::scratch()?;
        let (_s, server) = super::super::store::scratch()?;
        let long = format!("{{\"type\":\"assistant\",\"text\":\"{}\"}}", "the agent read the file and wrote a patch ".repeat(40));
        runner.put(vec![line("a1", "s-1", 0, "{\"type\":\"summary\"}"), line("a1", "s-1", 1, &long)]);
        runner.sync()?;
        let records = |c: &Changes| -> Vec<(bool, bool)> { c.rows.iter().filter_map(|c| if let super::super::rows::Row::Record(r) = &c.row { Some((r.kept.is_some(), r.line.is_empty())) } else { None }).collect() };
        // As kept: the long line goes compressed, the short one as it is.
        assert_eq!(records(&runner.changes_as(0, 1 << 20, true)?), vec![(false, false), (true, true)]);
        // A taker from before never says `kept`, and gets text.
        assert_eq!(records(&runner.changes_as(0, 1 << 20, false)?), vec![(false, false), (false, false)]);
        assert_eq!(decode(r#"{"t":"from","store":"s","rev":1,"zip":true}"#), Some(Msg::From { store: "s".into(), rev: 1, zip: true, kept: false, about: None }));
        assert_eq!(encode(&Msg::From { store: "s".into(), rev: 1, zip: true, kept: false, about: None }), r#"{"t":"from","store":"s","rev":1,"zip":true}"#);
        // The server, which takes them so, keeps them and reads them back the same.
        feed_all(&runner, &server, "box", 1 << 20)?;
        assert_eq!(server.record("a1", "s-1", 0, 10)?, vec![(0, "{\"type\":\"summary\"}".to_owned()), (1, long)]);
        Ok(())
    }

    /// Goal 5, plan 01a1a59c2aae (its top goal): an intake's telemetry of its feeds is written by its thread
    /// alone and read with no lock, and is there by the time a message is answered. A copy to restore from is
    /// taken as said. What a mirror sent back is read, from every shard at once, while it's written, and holds
    /// every range added.
    #[test]
    fn g5_an_intakes_telemetry_and_what_it_reads_per_batch_take_no_lock() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (_dir, server) = super::super::store::scratch()?;
            let (intake, answers) = Intake::start(server, Brings::TheirMachine, None)?;
            anyhow::ensure!(intake.peers().is_empty(), "telemetry before any feed");
            intake.heard(7, "box", Msg::Hello { store: "r1".into(), restore: false }, Vec::new());
            let answer = crate::rt::timeout(Duration::from_secs(5), answers.recv()).await??;
            anyhow::ensure!(answer.peer == 7 && matches!(decode(&answer.json), Some(Msg::From { .. })), "{answer:?}");
            let seen = intake.peers();
            anyhow::ensure!(seen.iter().any(|p| p.machine == "box" && p.store == "r1" && p.hello_at > 0), "its hello isn't in the telemetry: {seen:?}");
            intake.accept_restore("a-copy");
            anyhow::ensure!(intake.restores.load().contains("a-copy"), "a copy to restore from, said, isn't taken");
            let echoes = Arc::new(Echoes::default());
            let mut reading = Vec::new();
            for _ in 0..8 {
                let e = Arc::clone(&echoes);
                reading.push(crate::rt::spawn_spread_joined(async move { (0..1_000u64).filter(|rev| e.has("m", *rev)).count() }));
            }
            for n in 0..100u64 {
                echoes.add("m", n * 10, n * 10 + 4);
            }
            for r in reading {
                r.await.map_err(|_| anyhow::anyhow!("a reader ended"))?;
            }
            anyhow::ensure!((0..100u64).all(|n| echoes.has("m", n * 10) && echoes.has("m", n * 10 + 4) && !echoes.has("m", n * 10 + 5)), "a range added is missing");
            Ok(())
        })
    }

    #[test]
    fn what_a_mirror_sent_back_is_known_by_the_numbers_it_was_written_under() {
        let e = Echoes::default();
        e.add("m", 5, 9);
        e.add("m", 10, 12);
        e.add("m", 20, 20);
        e.add("other", 1, 100);
        assert!(e.has("m", 5) && e.has("m", 9) && e.has("m", 10) && e.has("m", 12) && e.has("m", 20));
        assert!(!e.has("m", 4) && !e.has("m", 13) && !e.has("m", 19) && !e.has("m", 21));
        assert!(!e.has("nobody", 5));
    }

    #[test]
    fn a_shipper_leaves_out_what_the_taker_has_and_still_goes_through_it() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (_r, runner) = super::super::store::scratch()?;
            let (_s, server) = super::super::store::scratch()?;
            for i in 1..=3u64 {
                runner.send("a1", &said(&format!("m{i}"), i, "x"));
            }
            runner.sync()?;
            let (ship, out) = Shipper::start_skipping(runner.clone(), std::sync::Arc::new(|rev| rev == 2));
            ship.link(Link::Up);
            let mut partial = HashMap::new();
            let at = || server.peer(&peer_name("box", runner.id()));
            for _ in 0..50 {
                let Ok(Ok((json, data))) = crate::rt::timeout(Duration::from_secs(5), out.recv()).await else { break };
                for m in took(&server, &mut partial, 7, "box", Brings::TheirMachine, &json, data, &Restores::default(), None) {
                    ship.link(Link::Heard(m));
                }
                if at()? == 3 {
                    break;
                }
            }
            assert_eq!(at()?, 3, "the taker is through the row left out");
            let ids: Vec<String> = server.kept("a1", 10)?.map(|k| k.events.into_iter().map(|e| e.id).collect()).unwrap_or_default();
            assert_eq!(ids, vec!["m1".to_owned(), "m3".to_owned()]);
            Ok(())
        })
    }

    /// Goal 9, plan 01a1a87b2cec step 3: what a mirror has already is left out of a batch on the compute pool, in the
    /// job that writes its JSON and digest, its rows let go of there, not asked of row by row on the feed's shard; and
    /// the feed takes its turns between what waits in its queue. What it leaves out, and that its taker still goes
    /// through it, is `a_shipper_leaves_out_what_the_taker_has_and_still_goes_through_it`'s.
    #[test]
    fn g9_s3_a_batch_is_left_out_of_off_the_shard() {
        let code: String = include_str!("feed.rs").split("\n#[cfg(test)]\nmod tests {").next().unwrap_or_default().lines().map(|l| l.split("//").next().unwrap_or_default()).collect::<Vec<_>>().join("\n");
        let ship = code.split("async fn ship(").nth(1).and_then(|rest| rest.split("\n}\n").next()).unwrap_or_default();
        let (on_shard, in_job) = ship.split_once("crate::rt::compute(move || -> anyhow::Result<(Vec<u8>, String, usize)>").unwrap_or_default();
        assert!(!on_shard.is_empty() && !on_shard.contains("skip(c.rev)") && in_job.contains("skip(c.rev)"), "{ship}");
        assert!(on_shard.contains("turns.after().await"), "its turns: {ship}");
    }

    /// A batch of a few hundred rows (events and a long record line), as a feed reads it from a store.
    fn a_batch() -> anyhow::Result<Changes> {
        let (_r, runner) = super::super::store::scratch()?;
        runner.send("a1", &Update::State { state: Box::new(State { title: "parser".into(), ..State::default() }) });
        for i in 1..=300u64 {
            runner.send("a1", &said(&format!("m{i}"), i, &format!("reply {i} {}", "x".repeat(700))));
        }
        runner.put(vec![line("a1", "s-1", 0, &format!("{{\"type\":\"user\",\"blob\":\"{}\"}}", "y".repeat(100_000)))]);
        runner.sync()?;
        runner.changes(0, 64 << 20)
    }

    /// What [`decode_batch`] makes of `json`, sent as it is and compressed: the same both ways.
    fn decoded(json: &[u8], check: Option<Option<String>>) -> anyhow::Result<Result<(Changes, Option<String>), String>> {
        let as_is = decode_batch(json.to_vec(), false, check.clone());
        let zipped = decode_batch(zip(json)?, true, check);
        let read = |d: Decoded| match d {
            Decoded::Read(b, sha) => Ok((b, sha)),
            Decoded::NotAsHanded(sha) => Err(format!("not as handed on: {sha}")),
            Decoded::Unreadable(e) => Err(format!("unreadable: {e:#}")),
        };
        let (as_is, zipped) = (read(as_is), read(zipped));
        anyhow::ensure!(format!("{as_is:?}") == format!("{zipped:?}"), "as it is {as_is:?}, compressed {zipped:?}");
        Ok(as_is)
    }

    /// Goal 3, plan 01a1a59c2a8e step 3: a whole batch decoded on the compute pool, a row at a time, reads
    /// as serde read it whole, sent as it is or compressed: as a feed writes it, its members the other way
    /// round or with one kiln doesn't know, spaced out; and where it can't be read, its error is serde's.
    #[test]
    fn g3_s3_a_batch_decoded_in_pieces_reads_as_it_did_whole() -> anyhow::Result<()> {
        let batch = a_batch()?;
        anyhow::ensure!(batch.rows.len() > 300, "{} rows", batch.rows.len());
        let json = serde_json::to_vec(&batch)?;
        anyhow::ensure!(decoded(&json, None)? == Ok((batch.clone(), None)));
        let rows = serde_json::to_string(&batch.rows)?;
        for shape in [
            format!("{{\"upto\":{},\"rows\":{rows}}}", batch.upto),
            format!(" {{ \"rows\" :\n {rows} ,\t\"later\": [1, {{\"x\": \"]\"}}], \"upto\" : {} }} \n", batch.upto),
            format!("{{\"rows\":[],\"upto\":{}}}", batch.upto),
        ] {
            let whole: Changes = serde_json::from_slice(shape.as_bytes())?;
            anyhow::ensure!(decoded(shape.as_bytes(), None)? == Ok((whole, None)), "{}", shape.chars().take(80).collect::<String>());
        }
        // Not as a feed writes it: read whole, so the outcome is serde's, error and all.
        for odd in [
            format!("{{\"rows\":{rows}}}"),
            format!("{{\"rows\":{rows},\"rows\":[],\"upto\":1}}"),
            format!("{{\"rows\":{rows},\"upto\":1}} trailing"),
            "{\"rows\":{},\"upto\":1}".to_owned(),
            "{\"rows\":[{\"rev\":1}],\"upto\":1}".to_owned(),
            format!("[{rows},{}]", batch.upto),
            "{\"rows\":[".to_owned(),
            String::new(),
        ] {
            let whole = serde_json::from_slice::<Changes>(odd.as_bytes()).map(|b| (b, None)).map_err(|e| format!("unreadable: {e}"));
            anyhow::ensure!(decoded(odd.as_bytes(), None)? == whole, "{}", odd.chars().take(80).collect::<String>());
        }
        Ok(())
    }

    /// Goal 3, plan 01a1a59c2a8e step 3, and W27-R08: a batch is checked against its giver's hand-on before
    /// it's parsed, by the SHA-256 of its JSON as it came unzipped: as handed on, read, its digest kept; not,
    /// refused, with the digest of what came; a hand-on that says nothing readable refuses it too.
    #[test]
    fn g3_s3_a_batch_is_checked_against_its_hand_on_before_it_is_parsed() -> anyhow::Result<()> {
        let batch = a_batch()?;
        let json = serde_json::to_vec(&batch)?;
        let sha = crate::util::sha256_hex(&json);
        anyhow::ensure!(decoded(&json, Some(Some(sha.clone())))? == Ok((batch, Some(sha.clone()))));
        anyhow::ensure!(decoded(&json, Some(Some("0".repeat(64))))? == Err(format!("not as handed on: {sha}")));
        anyhow::ensure!(decoded(&json, Some(None))? == Err(format!("not as handed on: {sha}")));
        // Checked before it's parsed: what isn't JSON at all, and isn't as handed on, is refused, not unreadable.
        let junk = b"not json".to_vec();
        anyhow::ensure!(decoded(&junk, Some(None))? == Err(format!("not as handed on: {}", crate::util::sha256_hex(&junk))));
        Ok(())
    }

    /// Goal 3, plan 01a1a59c2a8e step 3: a batch's decoding is many small units, so its piece can end at
    /// any of them (where another job waits for the pool's thread): a buffer unzipped or hashed at a time,
    /// a row at a time; none of them larger than a buffer of the JSON or one row.
    #[test]
    fn g3_s3_a_batchs_decoding_is_small_units() -> anyhow::Result<()> {
        let batch = a_batch()?;
        let json = serde_json::to_vec(&batch)?;
        let mut d = Decoding::new(zip(&json)?, true, Some(Some(crate::util::sha256_hex(&json))));
        let (mut units, mut biggest) = (0usize, 0usize);
        let read = loop {
            let (at, json_before) = (d.reading.at, d.json.len());
            let done = d.unit()?;
            units += 1;
            biggest = biggest.max((d.reading.at - at).max(d.json.len() - json_before));
            if let Some(done) = done {
                break done;
            }
            anyhow::ensure!(units < 10 * json.len(), "no end");
        };
        anyhow::ensure!(matches!(&read, Decoded::Read(b, Some(_)) if *b == batch), "{}", match read { Decoded::Read(..) => "read, but not the batch".to_owned(), Decoded::NotAsHanded(s) => s, Decoded::Unreadable(e) => format!("{e:#}") });
        // Unzipped and hashed a buffer at a time, then read a row at a time.
        let longest_row = batch.rows.iter().map(|c| serde_json::to_vec(c).map_or(0, |v| v.len())).max().unwrap_or(0);
        anyhow::ensure!(units >= 2 * json.len() / ZIP_BUFFER + batch.rows.len(), "{units} units for {} bytes and {} rows", json.len(), batch.rows.len());
        anyhow::ensure!(biggest <= ZIP_BUFFER.max(longest_row + 2), "a unit of {biggest} bytes");
        Ok(())
    }

    /// Goal 11, plan 01a1a87b2e56 step 5: a batch is zipped in a window no larger than it needs and at most a
    /// batch's size, past which a decoder's ring buffer doesn't grow, and comes back whole: a small batch, one
    /// of a batch's size, and one a large row took past it.
    #[test]
    fn g11_s5_a_batch_is_zipped_in_a_window_of_its_size() -> anyhow::Result<()> {
        anyhow::ensure!(zip_window(0) == 10 && zip_window(1000) == 10 && zip_window(20_000) == 15);
        anyhow::ensure!(zip_window(BATCH_BYTES) == 21 && zip_window(3 * BATCH_BYTES) == 21, "at most a batch's size");
        for len in [1000, BATCH_BYTES, BATCH_BYTES + BATCH_BYTES / 2] {
            let json: Vec<u8> = (0u64..).map(|i| format!("{{\"seq\":{i},\"text\":\"a line of the agent's {}\"}},", i % 977)).flat_map(String::into_bytes).take(len).collect();
            let mut back = Vec::new();
            brotli::BrotliDecompress(&mut zip(&json)?.as_slice(), &mut back)?;
            anyhow::ensure!(back == json, "{len} bytes came back as {}", back.len());
        }
        Ok(())
    }
}
