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
    /// its store.
    Hello { store: String },
    /// Server to runner: it has `store`'s feed through `rev`; send from there.
    From { store: String, rev: u64 },
    /// Runner to server: part `part` (from 0) of `parts` of a batch that follows `from` and reads
    /// through `upto` (its bytes: JSON [`Changes`]).
    Batch { store: String, from: u64, upto: u64, part: u32, parts: u32 },
    /// Server to runner: it has `store`'s feed through `rev`.
    Ack { store: String, rev: u64 },
}

pub fn encode(m: &Msg) -> String {
    serde_json::to_string(m).unwrap_or_default()
}

pub fn decode(json: &str) -> Option<Msg> {
    serde_json::from_str(json).ok()
}

/// A batch as the messages it travels in.
pub fn parts(store: &str, from: u64, batch: &Changes) -> anyhow::Result<Vec<(Msg, Vec<u8>)>> {
    let bytes = serde_json::to_vec(batch)?;
    let chunks: Vec<&[u8]> = if bytes.is_empty() { vec![&[][..]] } else { bytes.chunks(PART).collect() };
    let n = u32::try_from(chunks.len())?;
    Ok(chunks
        .into_iter()
        .enumerate()
        .map(|(i, c)| (Msg::Batch { store: store.to_owned(), from, upto: batch.upto, part: u32::try_from(i).unwrap_or(u32::MAX), parts: n }, c.to_vec()))
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
}

impl Shipper {
    pub fn start(store: Store) -> (Self, async_channel::Receiver<(String, Vec<u8>)>) {
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
        crate::rt::spawn(ship(store, rx, out_tx));
        (Self { tx }, out_rx)
    }

    pub fn link(&self, l: Link) {
        let _ = self.tx.try_send(l);
    }
}

/// Where the runner's feed stands on this link.
struct Shipping {
    /// What the server has, once it said (None: it hasn't yet, on this link).
    acked: Option<u64>,
    /// Read and sent through here.
    sent: u64,
    /// The batches sent and not yet acknowledged: their `upto`.
    flight: VecDeque<u64>,
    /// When the server last said something, or was last asked.
    heard: Instant,
}

async fn ship(store: Store, rx: async_channel::Receiver<Link>, out: async_channel::Sender<(String, Vec<u8>)>) {
    let me = store.id().to_owned();
    let mut up = false;
    let mut st = Shipping { acked: None, sent: 0, flight: VecDeque::new(), heard: Instant::now() };
    let hello = |out: &async_channel::Sender<(String, Vec<u8>)>| out.try_send((encode(&Msg::Hello { store: me.clone() }), Vec::new())).is_ok();
    loop {
        let ev = match crate::rt::timeout(Duration::from_secs(1), rx.recv()).await {
            Ok(Ok(ev)) => Some(ev),
            Ok(Err(_)) => return,
            Err(_) => None,
        };
        match ev {
            Some(Link::Up) => {
                up = true;
                st = Shipping { acked: None, sent: 0, flight: VecDeque::new(), heard: Instant::now() };
                hello(&out);
            }
            Some(Link::Down) => {
                up = false;
                st.acked = None;
                st.flight.clear();
            }
            Some(Link::Heard(Msg::From { store, rev })) if store == me => {
                // Where to go on from (a first answer, or after something went astray).
                st.acked = Some(rev);
                st.sent = rev;
                st.flight.clear();
                st.heard = Instant::now();
            }
            Some(Link::Heard(Msg::Ack { store, rev })) if store == me => {
                st.acked = Some(rev);
                st.flight.retain(|u| *u > rev);
                st.heard = Instant::now();
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
            hello(&out);
            continue;
        }
        if st.acked.is_none() {
            continue;
        }
        while st.flight.len() < WINDOW {
            let from = st.sent;
            let s = store.clone();
            let batch = match crate::rt::blocking(move || s.changes(from, BATCH_BYTES)).await {
                Ok(Ok(b)) => b,
                Ok(Err(e)) | Err(e) => {
                    tracing::debug!(error = %format!("{e:#}"), "conversation feed: the store couldn't be read");
                    break;
                }
            };
            if batch.rows.is_empty() {
                break;
            }
            let msgs = match parts(&me, from, &batch) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "conversation feed: a batch couldn't be encoded");
                    break;
                }
            };
            for (m, data) in msgs {
                if out.send((encode(&m), data)).await.is_err() {
                    return;
                }
            }
            if st.flight.is_empty() {
                st.heard = Instant::now();
            }
            st.sent = batch.upto;
            st.flight.push_back(batch.upto);
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

/// A batch being put back together from its parts.
struct Partial {
    from: u64,
    upto: u64,
    parts: u32,
    got: u32,
    bytes: Vec<u8>,
}

/// The server's side: one thread takes what runners' feeds send into the server's store, in the
/// order each sent it, and hands back what to answer.
pub struct Intake {
    tx: std::sync::mpsc::SyncSender<(hyper_transport::PeerId, String, String, Vec<u8>)>,
}

impl Intake {
    pub fn start(store: Store) -> anyhow::Result<(Self, async_channel::Receiver<Out>)> {
        let (tx, rx) = std::sync::mpsc::sync_channel::<(hyper_transport::PeerId, String, String, Vec<u8>)>(256);
        let (out_tx, out_rx) = async_channel::bounded::<Out>(1024);
        std::thread::Builder::new().name("kiln-convo-intake".into()).spawn(move || {
            let mut partial: HashMap<(hyper_transport::PeerId, String), Partial> = HashMap::new();
            while let Ok((peer, machine, json, data)) = rx.recv() {
                for m in take(&store, &mut partial, peer, &machine, &json, data) {
                    if out_tx.send_blocking(Out { peer, json: encode(&m) }).is_err() {
                        return;
                    }
                }
            }
        })?;
        Ok((Self { tx }, out_rx))
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

/// One message from a runner, taken: what to answer.
fn take(store: &Store, partial: &mut HashMap<(hyper_transport::PeerId, String), Partial>, peer: hyper_transport::PeerId, machine: &str, json: &str, data: Vec<u8>) -> Vec<Msg> {
    let have = |s: &str| store.peer(&peer_name(machine, s)).unwrap_or(0);
    match decode(json) {
        Some(Msg::Hello { store: s }) => {
            partial.remove(&(peer, s.clone()));
            let rev = have(&s);
            tracing::info!(%machine, store = %s, from = rev, "conversation feed: a runner's store sends the server's what it has after this");
            vec![Msg::From { store: s, rev }]
        }
        Some(Msg::Batch { store: s, from, upto, part, parts }) => {
            let key = (peer, s.clone());
            let fits = match partial.get(&key) {
                None => part == 0,
                Some(p) => p.from == from && p.upto == upto && p.parts == parts && p.got == part,
            };
            if !fits {
                // A part out of place (one went astray): start over from what the server has.
                partial.remove(&key);
                let rev = have(&s);
                return vec![Msg::From { store: s, rev }];
            }
            let p = partial.entry(key.clone()).or_insert_with(|| Partial { from, upto, parts, got: 0, bytes: Vec::new() });
            p.got += 1;
            p.bytes.extend_from_slice(&data);
            if p.bytes.len() > BATCH_MAX {
                partial.remove(&key);
                tracing::warn!(%machine, "conversation feed: a batch too large to take");
                return Vec::new();
            }
            if p.got < p.parts {
                return Vec::new();
            }
            let Some(done) = partial.remove(&key) else { return Vec::new() };
            let batch: Changes = match serde_json::from_slice(&done.bytes) {
                Ok(b) => b,
                Err(e) => {
                    tracing::warn!(%machine, error = %e, "conversation feed: an unreadable batch");
                    let rev = have(&s);
                    return vec![Msg::From { store: s, rev }];
                }
            };
            match store.take(&peer_name(machine, &s), machine, from, batch) {
                Ok(t) if t.gap => vec![Msg::From { store: s, rev: t.rev }],
                Ok(t) => {
                    if t.fenced > 0 {
                        tracing::info!(%machine, fenced = t.fenced, "conversation feed: rows of sessions that went on elsewhere were left out");
                    }
                    vec![Msg::Ack { store: s, rev: t.rev }]
                }
                Err(e) => {
                    // Not taken: the runner asks again after a while, and sends it again.
                    tracing::warn!(%machine, error = %format!("{e:#}"), "conversation feed: a batch couldn't be taken");
                    Vec::new()
                }
            }
        }
        Some(Msg::From { .. } | Msg::Ack { .. }) | None => Vec::new(),
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
    fn feed_all(runner: &Store, server: &Store, machine: &str, bytes: usize) -> anyhow::Result<u64> {
        let mut partial = HashMap::new();
        let hello = take(server, &mut partial, 7, machine, &encode(&Msg::Hello { store: runner.id().to_owned() }), Vec::new());
        let Some(Msg::From { mut rev, .. }) = hello.into_iter().next() else { anyhow::bail!("no answer to hello") };
        loop {
            let batch = runner.changes(rev, bytes)?;
            if batch.rows.is_empty() {
                return Ok(rev);
            }
            let mut answer = Vec::new();
            for (m, data) in parts(runner.id(), rev, &batch)? {
                answer = take(server, &mut partial, 7, machine, &encode(&m), data);
            }
            match answer.first() {
                Some(Msg::Ack { rev: r, .. }) => rev = *r,
                other => anyhow::bail!("the server answered {other:?}"),
            }
        }
    }

    fn line(session: &str, source: &str, n: u64, text: &str) -> super::super::rows::Row {
        super::super::rows::Row::Record(Box::new(super::super::rows::RecordRow { session: session.into(), source: source.into(), n, end: (n + 1) * 100, line: text.into() }))
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
    fn a_batch_that_does_not_follow_what_the_server_has_is_refused_with_where_it_stands() -> anyhow::Result<()> {
        let (_r, runner) = super::super::store::scratch()?;
        let (_s, server) = super::super::store::scratch()?;
        for i in 1..=3u64 {
            runner.send("a1", &said(&format!("m{i}"), i, "x"));
        }
        runner.sync()?;
        let batch = runner.changes(2, 1 << 20)?;
        let mut partial = HashMap::new();
        let mut answer = Vec::new();
        for (m, data) in parts(runner.id(), 2, &batch)? {
            answer = take(&server, &mut partial, 7, "box", &encode(&m), data);
        }
        assert_eq!(answer, vec![Msg::From { store: runner.id().to_owned(), rev: 0 }]);
        assert_eq!(server.kept("a1", 10)?, None);
        // A part out of place: the same.
        let (m, data) = parts(runner.id(), 0, &runner.changes(0, 1 << 20)?)?.into_iter().next().ok_or_else(|| anyhow::anyhow!("no part"))?;
        let Msg::Batch { store, from, upto, parts: n, .. } = m else { anyhow::bail!("not a batch") };
        let astray = Msg::Batch { store, from, upto, part: 1, parts: n + 1 };
        assert_eq!(take(&server, &mut partial, 7, "box", &encode(&astray), data), vec![Msg::From { store: runner.id().to_owned(), rev: 0 }]);
        Ok(())
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

    #[test]
    fn the_shipper_sends_what_the_store_writes_and_starts_over_after_a_gap() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let (_r, runner) = super::super::store::scratch()?;
            let (_s, server) = super::super::store::scratch()?;
            let (ship, out) = Shipper::start(runner.clone());
            ship.link(Link::Up);
            let mut partial = HashMap::new();
            // The server's side, in this test: what the shipper sends is taken, answers go back.
            let answer = |json: String, data: Vec<u8>, partial: &mut HashMap<(hyper_transport::PeerId, String), Partial>| {
                for m in take(&server, partial, 7, "box", &json, data) {
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
}
