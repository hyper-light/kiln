//! A machine's conversation store (`docs/conversations.md`, "The store"): one writer thread owns the
//! database; sessions' updates reach it by channel, never by lock, and are committed in batches, so
//! an update is durable within [`COMMIT_MS`] of its flush. Reads go through the same thread, in
//! turn with the writes.
//!
//! The engine is behind [`Engine`]: Turso on a machine ([`super::turso`]), a map in tests.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use super::rows::{self, EventRow, Row};
use crate::harness::core::Update;
use crate::harness::session::Sink;

/// How long a row waits for others to share its commit.
pub const COMMIT_MS: u64 = 50;
/// Rows that make a commit at once, however soon.
pub const COMMIT_ROWS: usize = 2048;

/// What the store keeps of one session.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Kept {
    /// Each part as JSON, as the model serializes it.
    pub info: Option<String>,
    pub meta: Option<String>,
    pub state: Option<String>,
    pub changes: Option<String>,
    pub updated_at: i64,
    pub closed_at: Option<i64>,
    /// Its events, oldest first.
    pub events: Vec<EventRow>,
}

/// What to look for (`GET /api/conversations/search`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Query {
    /// Words the event's text holds (each one, any case); empty: any.
    #[serde(default)]
    pub q: String,
    #[serde(default)]
    pub session: Option<String>,
    #[serde(default)]
    pub kind: Option<String>,
    #[serde(default)]
    pub tool: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub since: Option<i64>,
    #[serde(default)]
    pub until: Option<i64>,
    #[serde(default)]
    pub limit: Option<usize>,
}

/// Most results one search returns.
pub const SEARCH_MAX: usize = 500;

/// A database the store keeps conversations in.
pub trait Engine: Send + 'static {
    /// The rows, in one transaction.
    fn apply(&mut self, rows: &[Row]) -> anyhow::Result<()>;
    /// What is kept of `session` (None: nothing). `last`: at most its last this many events.
    fn kept(&mut self, session: &str, last: usize) -> anyhow::Result<Option<Kept>>;
    /// A session's events before `before` (its event numbers), the newest `limit`, oldest first.
    fn events(&mut self, session: &str, before: Option<u64>, limit: usize) -> anyhow::Result<Vec<EventRow>>;
    /// Events matching `q`, newest first.
    fn search(&mut self, q: &Query) -> anyhow::Result<Vec<EventRow>>;
}

enum Msg {
    Rows(Vec<Row>),
    /// Run on the engine, between commits (whatever was sent before it is committed first).
    Read(Box<dyn FnOnce(&mut dyn Engine) + Send>),
}

/// A handle to a machine's store: cheap to clone, usable from any thread.
#[derive(Clone)]
pub struct Store {
    tx: mpsc::Sender<Msg>,
}

impl Store {
    /// The store, its writer thread started on `engine`.
    pub fn start(engine: Box<dyn Engine>) -> anyhow::Result<Self> {
        let (tx, rx) = mpsc::channel::<Msg>();
        std::thread::Builder::new().name("kiln-convo".into()).spawn(move || writer(engine, &rx))?;
        Ok(Self { tx })
    }

    /// What `update` of `session` makes, to be committed with the next batch.
    pub fn send(&self, session: &str, update: &Update) {
        let rows = rows::of(session, update, crate::util::now_ms());
        if !rows.is_empty() {
            let _ = self.tx.send(Msg::Rows(rows));
        }
    }

    /// Rows made elsewhere (a replica's entries), to be committed with the next batch.
    pub fn put(&self, rows: Vec<Row>) {
        if !rows.is_empty() {
            let _ = self.tx.send(Msg::Rows(rows));
        }
    }

    /// Run `f` on the engine once what was sent before is committed. Blocking.
    fn read<T: Send + 'static>(&self, f: impl FnOnce(&mut dyn Engine) -> anyhow::Result<T> + Send + 'static) -> anyhow::Result<T> {
        let (rtx, rrx) = mpsc::channel();
        self.tx
            .send(Msg::Read(Box::new(move |e| {
                let _ = rtx.send(f(e));
            })))
            .map_err(|_| anyhow::anyhow!("the conversation store has stopped"))?;
        rrx.recv().map_err(|_| anyhow::anyhow!("the conversation store has stopped"))?
    }

    /// What is kept of `session`, with at most its last `last` events. Blocking.
    pub fn kept(&self, session: &str, last: usize) -> anyhow::Result<Option<Kept>> {
        let s = session.to_owned();
        self.read(move |e| e.kept(&s, last))
    }

    /// A session's events before event number `before`. Blocking.
    pub fn events(&self, session: &str, before: Option<u64>, limit: usize) -> anyhow::Result<Vec<EventRow>> {
        let s = session.to_owned();
        self.read(move |e| e.events(&s, before, limit))
    }

    /// Events matching `q`, newest first. Blocking.
    pub fn search(&self, q: Query) -> anyhow::Result<Vec<EventRow>> {
        self.read(move |e| e.search(&q))
    }

    /// Wait until everything sent so far is committed. Blocking.
    pub fn sync(&self) -> anyhow::Result<()> {
        self.read(|_| Ok(()))
    }
}

/// The writer: rows wait at most `COMMIT_MS` (or until `COMMIT_ROWS`) to share a commit; a read
/// commits what waits first, so it sees everything sent before it.
fn writer(mut engine: Box<dyn Engine>, rx: &mpsc::Receiver<Msg>) {
    let mut pending: Vec<Row> = Vec::new();
    let mut since: Option<Instant> = None;
    let commit = |engine: &mut Box<dyn Engine>, pending: &mut Vec<Row>| {
        if pending.is_empty() {
            return;
        }
        let rows = rows::coalesce(std::mem::take(pending));
        if let Err(e) = engine.apply(&rows) {
            tracing::warn!(error = %format!("{e:#}"), rows = rows.len(), "conversation store: a batch couldn't be written");
        }
    };
    loop {
        let wait = since.map_or(Duration::from_secs(3600), |t| Duration::from_millis(COMMIT_MS).saturating_sub(t.elapsed()));
        match rx.recv_timeout(wait) {
            Ok(Msg::Rows(rows)) => {
                pending.extend(rows);
                since.get_or_insert_with(Instant::now);
                if pending.len() >= COMMIT_ROWS {
                    commit(&mut engine, &mut pending);
                    since = None;
                }
            }
            Ok(Msg::Read(f)) => {
                commit(&mut engine, &mut pending);
                since = None;
                f(engine.as_mut());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                commit(&mut engine, &mut pending);
                since = None;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                commit(&mut engine, &mut pending);
                return;
            }
        }
    }
}

/// A sink that keeps what passes through it in `store` on its way to `sink`, in order, with the
/// same room (a full `sink` holds the sessions back, as it always did).
pub fn tee(sink: Sink, store: Store, room: usize) -> Sink {
    let (tx, rx) = async_channel::bounded::<(String, Update)>(room);
    crate::rt::spawn(async move {
        while let Ok((id, u)) = rx.recv().await {
            store.send(&id, &u);
            if sink.send((id, u)).await.is_err() {
                break;
            }
        }
    });
    tx
}

/// An engine in memory, for tests: what Turso keeps, without a file.
#[derive(Default)]
pub struct Memory {
    sessions: std::collections::BTreeMap<String, Kept>,
}

impl Engine for Memory {
    fn apply(&mut self, rows: &[Row]) -> anyhow::Result<()> {
        for row in rows {
            match row {
                Row::Session { session, info, meta, state, changes, at } => {
                    let k = self.sessions.entry(session.clone()).or_default();
                    for (slot, v) in [(&mut k.info, info), (&mut k.meta, meta), (&mut k.state, state), (&mut k.changes, changes)] {
                        if v.is_some() {
                            slot.clone_from(v);
                        }
                    }
                    k.updated_at = *at;
                }
                Row::Event(e) => {
                    let k = self.sessions.entry(e.session.clone()).or_default();
                    match k.events.iter_mut().find(|x| x.id == e.id) {
                        Some(x) => *x = (**e).clone(),
                        None => k.events.push((**e).clone()),
                    }
                    k.events.sort_by_key(|x| x.seq);
                }
                Row::Reset { session, .. } => {
                    if let Some(k) = self.sessions.get_mut(session) {
                        k.events.clear();
                    }
                }
                Row::Closed { session, at } => {
                    if let Some(k) = self.sessions.get_mut(session) {
                        k.closed_at = Some(*at);
                    }
                }
            }
        }
        Ok(())
    }

    fn kept(&mut self, session: &str, last: usize) -> anyhow::Result<Option<Kept>> {
        Ok(self.sessions.get(session).map(|k| {
            let mut k = k.clone();
            let skip = k.events.len().saturating_sub(last);
            k.events.drain(..skip);
            k
        }))
    }

    fn events(&mut self, session: &str, before: Option<u64>, limit: usize) -> anyhow::Result<Vec<EventRow>> {
        let Some(k) = self.sessions.get(session) else { return Ok(Vec::new()) };
        let older: Vec<EventRow> = k.events.iter().filter(|e| before.is_none_or(|b| e.seq < b)).cloned().collect();
        let skip = older.len().saturating_sub(limit);
        Ok(older.into_iter().skip(skip).collect())
    }

    fn search(&mut self, q: &Query) -> anyhow::Result<Vec<EventRow>> {
        let words: Vec<String> = q.q.split_whitespace().map(str::to_lowercase).collect();
        let mut hits: Vec<EventRow> = self
            .sessions
            .iter()
            .filter(|(s, _)| q.session.as_ref().is_none_or(|want| want == *s))
            .flat_map(|(_, k)| k.events.iter())
            .filter(|e| {
                let text = e.text.to_lowercase();
                words.iter().all(|w| text.contains(w.as_str()))
                    && q.kind.as_ref().is_none_or(|k| &e.kind == k)
                    && q.tool.as_ref().is_none_or(|t| e.tool.as_ref() == Some(t))
                    && q.status.as_ref().is_none_or(|s| e.status.as_ref() == Some(s))
                    && q.since.is_none_or(|s| e.at >= s)
                    && q.until.is_none_or(|u| e.at < u)
            })
            .cloned()
            .collect();
        hits.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| b.seq.cmp(&a.seq)));
        hits.truncate(q.limit.unwrap_or(100).min(SEARCH_MAX));
        Ok(hits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::harness::model::{Body, Event, State};

    fn said(id: &str, seq: u64, text: &str) -> Update {
        Update::Events { events: vec![Event { id: id.into(), seq, at: 1_000 + seq as i64, parent: None, by: None, body: Body::Assistant { text: text.into(), streaming: false } }] }
    }

    #[test]
    fn what_is_sent_is_kept_and_read_back_in_order() -> anyhow::Result<()> {
        let store = Store::start(Box::<Memory>::default())?;
        store.send("a1", &Update::State { state: Box::new(State { title: "parser".into(), ..State::default() }) });
        for i in 1..=50u64 {
            store.send("a1", &said(&format!("m{i}"), i, &format!("reply {i}")));
        }
        // An event changed later is kept as it is now.
        store.send("a1", &said("m3", 3, "reply three, revised"));
        let k = store.kept("a1", 10)?.ok_or_else(|| anyhow::anyhow!("nothing kept"))?;
        assert!(k.state.as_deref().is_some_and(|s| s.contains("parser")));
        assert_eq!(k.events.len(), 10);
        assert_eq!(k.events.first().map(|e| e.id.as_str()), Some("m41"));
        let older = store.events("a1", Some(41), 5)?;
        assert_eq!(older.iter().map(|e| e.seq).collect::<Vec<_>>(), vec![36, 37, 38, 39, 40]);
        let all = store.events("a1", None, 100)?;
        assert!(all.iter().any(|e| e.id == "m3" && e.text == "reply three, revised"));
        Ok(())
    }

    #[test]
    fn a_search_finds_words_newest_first() -> anyhow::Result<()> {
        let store = Store::start(Box::<Memory>::default())?;
        store.send("a1", &said("m1", 1, "the parser fails on tabs"));
        store.send("a2", &said("m5", 5, "Parser fixed"));
        store.send("a2", &said("m6", 6, "unrelated"));
        let hits = store.search(Query { q: "parser".into(), ..Query::default() })?;
        // Any case; the newer first.
        assert_eq!(hits.iter().map(|e| e.session.as_str()).collect::<Vec<_>>(), vec!["a2", "a1"]);
        let only = store.search(Query { q: "parser tabs".into(), ..Query::default() })?;
        assert_eq!(only.len(), 1);
        Ok(())
    }

    #[test]
    fn a_reset_drops_the_events_before_it() -> anyhow::Result<()> {
        let store = Store::start(Box::<Memory>::default())?;
        store.send("a1", &said("m1", 1, "one"));
        store.send("a1", &said("m2", 2, "two"));
        store.send("a1", &Update::Reset);
        store.send("a1", &said("m1", 1, "one"));
        let k = store.kept("a1", 100)?.ok_or_else(|| anyhow::anyhow!("nothing kept"))?;
        assert_eq!(k.events.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), vec!["m1"]);
        Ok(())
    }

    #[test]
    fn the_tee_keeps_and_passes_on_in_order() -> anyhow::Result<()> {
        crate::rt::block_on(async move {
            let store = Store::start(Box::<Memory>::default())?;
            let (sink, out) = async_channel::bounded(4);
            let tee = tee(sink, store.clone(), 4);
            for i in 1..=3u64 {
                tee.send(("a1".to_owned(), said(&format!("m{i}"), i, "x"))).await?;
            }
            for i in 1..=3u64 {
                let (id, u) = out.recv().await?;
                assert_eq!(id, "a1");
                assert!(matches!(u, Update::Events { events } if events.first().map(|e| e.seq) == Some(i)));
            }
            store.sync()?;
            assert_eq!(store.kept("a1", 10)?.map(|k| k.events.len()), Some(3));
            Ok(())
        })
    }
}
