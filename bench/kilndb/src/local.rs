//! KilnDB as it is today: one machine's store (kiln's `convo::store`), in this process. A write is
//! handed to the store's writer and waited for until it is committed.

use std::collections::HashSet;
use std::sync::Mutex;

use kiln::convo::rows::{EventRow, Row};
use kiln::convo::store::{Query, Store, Upkeep};

use crate::{Driver, Event};

pub struct Local {
    store: Store,
    /// Sessions written so far: a session's row goes before its first event.
    seen: Mutex<HashSet<String>>,
}

impl Local {
    pub fn open(dir: &str) -> anyhow::Result<Self> {
        let dir = std::path::Path::new(dir);
        let _ = std::fs::remove_dir_all(dir);
        std::fs::create_dir_all(dir)?;
        let store = kiln::convo::open_file(dir, "bench.db", Upkeep::default()).ok_or_else(|| anyhow::anyhow!("the store couldn't be opened in {}", dir.display()))?;
        Ok(Self { store, seen: Mutex::new(HashSet::new()) })
    }
}

impl Driver for Local {
    fn write(&self, events: &[Event]) -> anyhow::Result<()> {
        let mut rows = Vec::with_capacity(events.len() + 1);
        {
            let mut seen = self.seen.lock().map_err(|_| anyhow::anyhow!("poisoned"))?;
            for e in events {
                if seen.insert(e.session.clone()) {
                    rows.push(Row::Session { session: e.session.clone(), info: Some("{}".into()), meta: None, state: None, changes: None, epoch: None, open: true, at: e.at });
                }
            }
        }
        rows.extend(events.iter().map(|e| {
            Row::Event(Box::new(EventRow {
                session: e.session.clone(),
                id: e.id.clone(),
                seq: e.seq,
                at: e.at,
                parent: None,
                by: None,
                kind: e.kind.to_owned(),
                level: None,
                tool: None,
                status: None,
                text: e.text.clone(),
                body: e.body.clone(),
            }))
        }));
        anyhow::ensure!(self.store.put_wait(rows), "the store's writer didn't take the rows");
        // Committed: what was handed over before this is in the file.
        self.store.sync()
    }

    fn history(&self, session: &str, limit: usize) -> anyhow::Result<usize> {
        Ok(self.store.events(session, None, limit)?.len())
    }

    fn search(&self, word: &str, limit: usize) -> anyhow::Result<usize> {
        Ok(self.store.search(Query { q: word.to_owned(), limit: Some(limit), ..Query::default() })?.len())
    }
}
