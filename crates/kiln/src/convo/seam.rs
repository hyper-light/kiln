//! kiln's conversation store behind kiln-seams' Conversations seam (docs/seams.md; CP-T1.1): a [`Store`] on either
//! engine ([`super::Kind`]: Turso, the control, or Amphora's), held to the seam's conformance suite and run through
//! its load scenario, each engine the same way.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use kiln_seams::conversations::{Changed, Conversations, Entry, Error, Page};
use parking_lot::Mutex;

use super::rows::{self, Row};
use super::store::Store;
use crate::harness::core::Update;
use crate::harness::model::{Body, Event};

/// A [`Store`] as the seam's Conversations: an entry is an assistant's event of its session.
pub struct StoreConversations {
    store: Store,
    /// Sessions given their row: a session's row goes before its first event, as kiln's harness writes it.
    opened: Mutex<HashSet<String>>,
    closed: AtomicBool,
}

impl StoreConversations {
    pub fn new(store: Store) -> Self {
        Self { store, opened: Mutex::new(HashSet::new()), closed: AtomicBool::new(false) }
    }

    fn live(&self) -> Result<(), Error> {
        if self.closed.load(Ordering::Acquire) { Err(Error::Closed) } else { Ok(()) }
    }
}

fn failed(e: anyhow::Error) -> Error {
    Error::Failed(format!("{e:#}"))
}

fn entry_of(r: &rows::EventRow) -> Entry {
    Entry { conversation: r.session.clone(), id: r.id.clone(), seq: r.seq, text: r.text.clone() }
}

impl Conversations for StoreConversations {
    fn append(&self, entries: &[Entry]) -> Result<(), Error> {
        self.live()?;
        let now = crate::util::now_ms();
        let mut out = Vec::with_capacity(entries.len().saturating_add(1));
        {
            let mut opened = self.opened.lock();
            for e in entries {
                if opened.insert(e.conversation.clone()) {
                    out.push(Row::Session { session: e.conversation.clone(), info: None, meta: None, state: None, changes: None, epoch: None, open: true, at: now });
                }
                let event = Event { id: e.id.clone(), seq: e.seq, at: i64::try_from(e.seq).unwrap_or(i64::MAX), parent: None, by: None, body: Body::Assistant { text: e.text.clone(), streaming: false } };
                out.extend(rows::of(&e.conversation, &Update::Events { events: vec![event] }, now));
            }
        }
        if !self.store.put_wait(out) {
            return Err(Error::Failed("the conversation store's writer has stopped".into()));
        }
        self.store.sync().map_err(failed)
    }

    fn history(&self, conversation: &str, before: Option<u64>, limit: usize) -> Result<Vec<Entry>, Error> {
        self.live()?;
        Ok(self.store.events(conversation, before, limit).map_err(failed)?.iter().map(entry_of).collect())
    }

    fn changes(&self, from: u64, max_bytes: usize) -> Result<Page, Error> {
        self.live()?;
        // The store's feed hands on its sessions' rows too: past a page of them alone, on to the next.
        let mut at = from;
        loop {
            let batch = self.store.changes(at, max_bytes).map_err(failed)?;
            let changes: Vec<Changed> = batch.rows.iter().filter_map(|c| if let Row::Event(e) = &c.row { Some(Changed { rev: c.rev, entry: entry_of(e) }) } else { None }).collect();
            let more = !batch.rows.is_empty() && batch.upto > at;
            at = batch.upto.max(at);
            if !changes.is_empty() || !more {
                return Ok(Page { changes, upto: at });
            }
        }
    }

    fn top(&self) -> Result<u64, Error> {
        self.live()?;
        self.store.top().map_err(failed)
    }

    fn close(&self) -> Result<(), Error> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.store.close().map_err(failed)
    }
}

/// A store on `kind`'s engine in a folder of its own, opened as often as a case asks.
#[cfg(test)]
pub struct Fixture {
    pub kind: super::Kind,
    /// Its folder (None: none could be made, and opening says so).
    pub dir: Option<tempfile::TempDir>,
}

#[cfg(test)]
impl kiln_seams::conversations::ConversationsFixture for Fixture {
    fn open(&self) -> Result<Arc<dyn Conversations>, String> {
        let dir = self.dir.as_ref().ok_or_else(|| "no folder for the store".to_owned())?;
        let engine = self.kind.open(&dir.path().join(super::FILE)).map_err(|e| format!("{e:#}"))?;
        let store = Store::start(engine, super::store::Upkeep::default()).map_err(|e| format!("{e:#}"))?;
        Ok(Arc::new(StoreConversations::new(store)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_seams::conversations::{Load, cases, load};

    /// CP-T1.1: the conversation store passes every clause of the Conversations seam on each engine, Turso and
    /// Amphora's (the feed's exactly-once-in-order model test, CONVO-3 and CONVO-4, among them).
    #[test]
    fn cp_t1_1_the_store_passes_the_conversations_suite_on_each_engine() -> anyhow::Result<()> {
        for kind in super::super::Kind::ALL {
            let report = crate::rt::block_on(kiln_seams::conformance::run("Conversations", &cases::<Fixture>(), || {
                let dir = tempfile::tempdir().ok();
                async move { Fixture { kind, dir } }
            }));
            report.into_result().map_err(|e| anyhow::anyhow!("{kind:?}: {e}"))?;
        }
        Ok(())
    }

    fn knob(name: &str, default: usize) -> anyhow::Result<usize> {
        match std::env::var(name) {
            Ok(v) => Ok(v.trim().parse()?),
            Err(_) => Ok(default),
        }
    }

    /// CP-T1.1: the seam's load scenario on each engine, Turso the control: many conversations written at once, a
    /// burst of readers following the feed, a restart mid-stream; each reader's feed held to exactly once, in order,
    /// and what was measured printed a line an engine. Its scale comes from KILN_CONVO_LOAD_* (a tiny one unset).
    #[test]
    fn cp_t1_1_the_load_scenario_runs_on_each_engine() -> anyhow::Result<()> {
        let l = Load {
            conversations: knob("KILN_CONVO_LOAD_CONVERSATIONS", 16)?,
            writers: knob("KILN_CONVO_LOAD_WRITERS", 4)?,
            appends: knob("KILN_CONVO_LOAD_APPENDS", 25)?,
            readers: knob("KILN_CONVO_LOAD_READERS", 3)?,
            text_bytes: knob("KILN_CONVO_LOAD_TEXT_BYTES", 600)?,
            page_bytes: knob("KILN_CONVO_LOAD_PAGE_BYTES", 1 << 16)?,
            restart: knob("KILN_CONVO_LOAD_RESTART", 1)? != 0,
        };
        for kind in super::super::Kind::ALL {
            let r = load(&Fixture { kind, dir: Some(tempfile::tempdir()?) }, &l).map_err(|e| anyhow::anyhow!("{kind:?}: {e}"))?;
            anyhow::ensure!(r.appended == l.writers.saturating_mul(l.appends), "{kind:?}: {r:?}");
            println!("convo-load {kind:?} {l:?} {r:?}");
        }
        Ok(())
    }
}
