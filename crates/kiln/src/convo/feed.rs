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

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::store::{Changes, Store};
use super::telemetry::{PeerView, Report, ShipStats, ShipView};
use crate::proto;

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

fn zip(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    use std::io::Write as _;
    let mut out = Vec::with_capacity(bytes.len() / 4 + 16);
    {
        let mut w = brotli::CompressorWriter::new(&mut out, 4096, ZIP_QUALITY, 22);
        w.write_all(bytes)?;
        w.flush()?;
    }
    Ok(out)
}

fn unzip(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    use std::io::Read as _;
    let mut out = Vec::new();
    brotli::Decompressor::new(bytes, 4096).take(BATCH_MAX as u64 + 1).read_to_end(&mut out)?;
    anyhow::ensure!(out.len() <= BATCH_MAX, "a batch too large to take");
    Ok(out)
}

/// A batch as the messages it travels in, compressed when the taker takes that (`zipped`), sent
/// after being told to go on from `told` ([`Msg::Batch`]).
pub fn parts(store: &str, from: u64, batch: &Changes, zipped: bool, told: Option<u64>) -> anyhow::Result<Vec<(Msg, Vec<u8>)>> {
    let json = serde_json::to_vec(batch)?;
    let bytes = if zipped { zip(&json)? } else { json };
    let chunks: Vec<&[u8]> = if bytes.is_empty() { vec![&[][..]] } else { bytes.chunks(PART).collect() };
    let n = u32::try_from(chunks.len())?;
    Ok(chunks
        .into_iter()
        .enumerate()
        .map(|(i, c)| (Msg::Batch { store: store.to_owned(), from, upto: batch.upto, part: u32::try_from(i).unwrap_or(u32::MAX), parts: n, zip: zipped, told }, c.to_vec()))
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

/// A runner's feed: tell it of the link with [`Shipper::link`]; what it sends comes out of the
/// receiver [`Shipper::start`] returns, as `(json, bytes)` for [`proto::AgentMsg::Convo`].
#[derive(Clone)]
pub struct Shipper {
    tx: async_channel::Sender<Link>,
    stats: std::sync::Arc<ShipStats>,
}

/// Whether the taker has a row already (by its number): it is left out of what goes to it.
pub type Skip = std::sync::Arc<dyn Fn(u64) -> bool + Send + Sync>;

impl Shipper {
    pub fn start(store: Store) -> (Self, async_channel::Receiver<(String, Vec<u8>)>) {
        Self::start_as(store, false, None)
    }

    /// [`Shipper::start`], leaving out the rows `skip` says the taker has (what a mirror sent
    /// back to restore this store from isn't sent to it again, [`Echoes`]).
    pub fn start_skipping(store: Store, skip: Skip) -> (Self, async_channel::Receiver<(String, Vec<u8>)>) {
        Self::start_as(store, false, Some(skip))
    }

    /// A mirror's copy of the server's store, sent back for the server to restore its own from
    /// ([`Msg::Restore`]): everything the copy holds now, then [`Msg::Restored`], and the feed
    /// ends (what it takes meanwhile, it takes from the server: not sent back).
    pub fn start_restore(store: Store) -> (Self, async_channel::Receiver<(String, Vec<u8>)>) {
        Self::start_as(store, true, None)
    }

    fn start_as(store: Store, restore: bool, skip: Option<Skip>) -> (Self, async_channel::Receiver<(String, Vec<u8>)>) {
        let (tx, rx) = async_channel::unbounded::<Link>();
        let (out_tx, out_rx) = async_channel::bounded::<(String, Vec<u8>)>(16);
        // Each commit, as a nudge in the same queue.
        let watch = store.watch();
        let nudge = tx.clone();
        crate::rt::spawn(async move {
            while watch.recv().await.is_ok() {
                if nudge.send(Link::Wrote).await.is_err() {
                    break;
                }
            }
        });
        let stats: std::sync::Arc<ShipStats> = std::sync::Arc::default();
        crate::rt::spawn(ship(store, rx, out_tx, restore, skip, stats.clone()));
        (Self { tx, stats }, out_rx)
    }

    pub fn link(&self, l: Link) {
        let _ = self.tx.try_send(l);
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

async fn ship(store: Store, rx: async_channel::Receiver<Link>, out: async_channel::Sender<(String, Vec<u8>)>, restore: bool, skip: Option<Skip>, stats: std::sync::Arc<ShipStats>) {
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
    let hello = |out: &async_channel::Sender<(String, Vec<u8>)>| out.try_send((encode(&Msg::Hello { store: me.clone(), restore }), Vec::new())).is_ok();
    loop {
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
            // What the taker has already is left out; the batch still says through where it read.
            if let Some(skip) = &skip {
                batch.rows.retain(|c| !skip(c.rev));
            }
            let msgs = match parts(&me, from, &batch, st.zipped, st.told) {
                Ok(m) => m,
                Err(e) => {
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
            st.sent = batch.upto;
            st.flight.push_back((from, batch.upto));
            stats.sent(batch.upto, batch.rows.len(), bytes, st.flight.len());
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
    tx: std::sync::mpsc::SyncSender<(hyper_transport::PeerId, String, String, Vec<u8>)>,
    restores: std::sync::Arc<Restores>,
    peers: std::sync::Arc<Peers>,
}

/// Where each store that feeds an intake stands, by its name among the store's peers
/// ([`peer_name`]): for telemetry.
type Peers = std::sync::Mutex<HashMap<String, PeerView>>;

/// What each mirror sent back to restore a store, by the numbers it was written under there: the
/// feed to that mirror leaves it out, as the mirror has it already, in the copy it sent.
#[derive(Default)]
pub struct Echoes(std::sync::Mutex<HashMap<String, Vec<(u64, u64)>>>);

impl Echoes {
    /// Numbers `first` through `last` were written from `machine`'s copy.
    pub fn add(&self, machine: &str, first: u64, last: u64) {
        let mut all = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let ranges = all.entry(machine.to_owned()).or_default();
        match ranges.last_mut() {
            Some(r) if r.1 + 1 == first => r.1 = last,
            _ => ranges.push((first, last)),
        }
    }

    /// Whether number `rev` was written from `machine`'s copy.
    pub fn has(&self, machine: &str, rev: u64) -> bool {
        let all = self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        all.get(machine).is_some_and(|ranges| {
            let i = ranges.partition_point(|r| r.1 < rev);
            ranges.get(i).is_some_and(|r| r.0 <= rev)
        })
    }
}

/// The mirrors' copies an intake takes to restore its store from ([`Intake::accept_restore`]).
type Restores = std::sync::Mutex<std::collections::HashSet<String>>;

impl Intake {
    /// `echoes`: where to note what a mirror's copy brought (the server's intake).
    pub fn start(store: Store, brings: Brings, echoes: Option<std::sync::Arc<Echoes>>) -> anyhow::Result<(Self, async_channel::Receiver<Out>)> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<(hyper_transport::PeerId, String, String, Vec<u8>)>(256);
        let (out_tx, out_rx) = async_channel::bounded::<Out>(1024);
        let restores: std::sync::Arc<Restores> = std::sync::Arc::default();
        let peers: std::sync::Arc<Peers> = std::sync::Arc::default();
        let (restoring, seen) = (restores.clone(), peers.clone());
        std::thread::Builder::new().name("kiln-convo-intake".into()).spawn(move || {
            let mut taking: HashMap<(hyper_transport::PeerId, String), Taking> = HashMap::new();
            while let Ok((peer, machine, json, data)) = rx.recv() {
                let feed = Feed { store: &store, brings, restores: &restoring, echoes: echoes.as_deref(), peers: &seen };
                for m in take(&feed, &mut taking, peer, &machine, &json, data) {
                    if out_tx.send_blocking(Out { peer, json: encode(&m) }).is_err() {
                        return;
                    }
                }
            }
        })?;
        Ok((Self { tx, restores, peers }, out_rx))
    }

    /// Where each store that feeds this intake stands, since kiln started.
    pub fn peers(&self) -> Vec<PeerView> {
        self.peers.lock().unwrap_or_else(std::sync::PoisonError::into_inner).values().cloned().collect()
    }

    /// Take store `store`'s feed as a mirror's copy of this one, sent back to restore it
    /// ([`super::store::Store::take_copy`]): what it brings fills in what this store lacks.
    pub fn accept_restore(&self, store: &str) {
        self.restores.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(store.to_owned());
    }

    /// What runner process `peer` of `machine` sent on the feed's lane. Never blocks: when the
    /// intake is behind, it's dropped (the runner asks again, [`STALL`]).
    pub fn heard(&self, peer: hyper_transport::PeerId, machine: &str, json: String, data: Vec<u8>) {
        let _ = self.tx.try_send((peer, machine.to_owned(), json, data));
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
}

impl Feed<'_> {
    /// Telemetry of store `store` of `machine`.
    fn note(&self, machine: &str, store: &str, f: impl FnOnce(&mut PeerView)) {
        let mut all = self.peers.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let v = all.entry(peer_name(machine, store)).or_insert_with(|| PeerView { machine: machine.to_owned(), store: store.to_owned(), ..PeerView::default() });
        f(v);
    }
}

/// One message from a runner, taken: what to answer.
fn take(feed: &Feed<'_>, taking: &mut HashMap<(hyper_transport::PeerId, String), Taking>, peer: hyper_transport::PeerId, machine: &str, json: &str, data: Vec<u8>) -> Vec<Msg> {
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
    match decode(json) {
        Some(Msg::Hello { store: s, .. }) => {
            taking.remove(&(peer, s.clone()));
            feed.note(machine, &s, |v| v.hello_at = crate::util::now_ms());
            let Some(rev) = stands(&s) else { return Vec::new() };
            tracing::info!(%machine, store = %s, from = rev, "conversation feed: a runner's store sends the server's what it has after this");
            taking.entry((peer, s.clone())).or_default().told = Some(rev);
            vec![Msg::From { store: s, rev, zip: true, kept: true, about: None }]
        }
        Some(Msg::Batch { store: s, from, upto, part, parts, zip, told }) => {
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
            let p = t.batch.get_or_insert_with(|| Partial { from, upto, parts, got: 0, zip, bytes: Vec::new() });
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
            let read = if done.zip { unzip(&done.bytes) } else { Ok(done.bytes) };
            let batch: Changes = match read.and_then(|b| serde_json::from_slice(&b).map_err(anyhow::Error::from)) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(%machine, error = %format!("{e:#}"), "conversation feed: an unreadable batch");
                    feed.note(machine, &s, |v| v.errors += 1);
                    let rev = stands_now(taking);
                    return tell(taking, rev);
                }
            };
            let theirs = (brings == Brings::TheirMachine).then_some(machine);
            let copy = restores.lock().unwrap_or_else(std::sync::PoisonError::into_inner).contains(&s);
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
        Some(Msg::From { .. } | Msg::Ack { .. } | Msg::Restore { .. } | Msg::Restored { .. } | Msg::Stats { .. }) | None => Vec::new(),
    }
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
        take(&Feed { store, brings, restores, echoes, peers: &peers }, partial, peer, machine, json, data)
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
        let astray = |told| (Msg::Batch { store: store.clone(), from: f, upto, part: 1, parts: n + 1, zip: true, told }, data.clone());
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
        assert_eq!(answer(Msg::Batch { store: "r".into(), from: 0, upto: 5, part: 1, parts: 2, zip: false, told: Some(0) }, b"{}"), Vec::new());
        assert_eq!(answer(Msg::Batch { store: "r".into(), from: 0, upto: 5, part: 0, parts: 1, zip: false, told: Some(0) }, b"not json"), Vec::new());
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
            restores.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(mirror.id().to_owned());
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

    /// What the shipper sends next, within `wait` (None: nothing).
    async fn next_sent(out: &async_channel::Receiver<(String, Vec<u8>)>, wait: Duration) -> Option<Msg> {
        crate::rt::timeout(wait, out.recv()).await.ok().and_then(Result::ok).and_then(|(json, _)| decode(&json))
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
}
