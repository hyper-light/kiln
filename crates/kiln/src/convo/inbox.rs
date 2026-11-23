//! Each session's inbox and journal in its runner's conversation store (Ada, 2026-11-20: each runner keeps its own
//! database of its conversations, replicated across; each session its own durable inbox and journal). The ids of the
//! messages a session took, its head and epoch, and the messages in its custody (`crate::harness::inbox`) are rows of
//! the store's `inbox` table (migration 5), beside the session's conversation, by session: written by the store's one
//! writer, numbered as its other writes are, so its feed carries them with the rest (docs/conversations.md).
//!
//! [`InboxRecords`] is the inbox's kiln-seams `Records` over that table. Each commit is one transaction, its writes under
//! one number, synced past the drive's cache before it's answered; reads go beside the writer, at what's committed.
//! The table keeps rows as they are now, no history: a read at a snapshot reads as one now, and the change feed is the
//! store's own (`changes` here refuses). `head` and `wait` count the commits made through it, in this process.
//!
//! A host that kept its sessions' inboxes in a file of their own before (`records.db`, [`crate::records::FILE`]) has
//! them brought over the first time it's asked anything, before anything is answered; the store's numbers go on past
//! that file's, so a take's number (`Answer::seq`) still rises, and the file is set aside (`records.db.brought`).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures::future::BoxFuture;
use kiln_seams::records::{Changes, Consistency, KEY_MAX, Page, Records, Refusal, Rev, Row, SCAN_MAX, Scan, TX_MAX, Tx, VALUE_MAX, Write};
use parking_lot::Mutex;

use super::store::{Held, Store};

/// The session a key of a family names: its id's bytes. None: the family isn't the inbox's, or the key names no session.
pub type Whose = for<'k> fn(&str, &'k [u8]) -> Option<&'k [u8]>;

/// What the inbox's families are ([`InboxRecords`]).
#[derive(Clone, Copy)]
pub struct Families {
    /// Whose each key is.
    pub whose: Whose,
    /// Every family the inbox keeps (what's brought over from its file of its own).
    pub all: &'static [&'static str],
}

/// Where the store is: a runner's, held for its sessions' updates (given as the runner starts or a new build takes over,
/// taken away as it hands over), or the server's own.
#[derive(Clone)]
pub enum Source {
    Held(Held),
    Store(Store),
}

impl Source {
    fn store(&self) -> Result<Store, Refusal> {
        match self {
            Source::Store(st) => Ok(st.clone()),
            Source::Held(h) => h.current().ok_or_else(|| Refusal::Unavailable("the conversation store isn't open here: its sessions' inboxes are in it".into())),
        }
    }
}

/// Each session's inbox and journal in the conversation store ([the module](self)).
pub struct InboxRecords {
    source: Source,
    families: Families,
    /// The inbox's file of its own from before, until its rows are brought over, and where it is.
    legacy: Mutex<Option<(Arc<dyn Records>, PathBuf)>>,
    bringing: futures::lock::Mutex<()>,
    /// Commits made through it, in this process: the store's numbers they were made under.
    head: AtomicU64,
    /// Each family's newest commit through it.
    newest: Mutex<HashMap<String, Rev>>,
    changed: crate::rt::Changed,
}

fn unavailable(e: &anyhow::Error) -> Refusal {
    Refusal::Unavailable(format!("{e:#}"))
}

pub(crate) fn session_ok(id: &[u8]) -> Option<&str> {
    let ok = !id.is_empty() && id.len() <= 64 && id.iter().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
    if ok { std::str::from_utf8(id).ok() } else { None }
}

impl InboxRecords {
    /// The inbox kept in `source`'s store, its families as `families` says; `legacy`, its file of its own from before at
    /// its path, brought over the first time it's asked anything.
    pub fn new(source: Source, families: Families, legacy: Option<(Arc<dyn Records>, PathBuf)>) -> Self {
        Self { source, families, legacy: Mutex::new(legacy), bringing: futures::lock::Mutex::new(()), head: AtomicU64::new(0), newest: Mutex::new(HashMap::new()), changed: crate::rt::Changed::new() }
    }

    /// The session each write of `tx` is of, and every key checked against what a record holds.
    fn sessions_of(&self, tx: &Tx) -> Result<Vec<String>, Refusal> {
        if tx.conds.len() > TX_MAX || tx.writes.len() > TX_MAX {
            return Err(Refusal::TooLarge { what: "a transaction", size: tx.conds.len().max(tx.writes.len()), limit: TX_MAX });
        }
        tx.writes
            .iter()
            .map(|w| {
                let (family, key, value) = match w {
                    Write::Put { family, key, value } => (family, key, Some(value)),
                    Write::Delete { family, key } => (family, key, None),
                };
                if key.is_empty() || key.len() > KEY_MAX {
                    return Err(Refusal::Invalid { what: "a key", why: format!("{} bytes: 1 to {KEY_MAX}", key.len()) });
                }
                if let Some(v) = value
                    && v.len() > VALUE_MAX
                {
                    return Err(Refusal::TooLarge { what: "a value", size: v.len(), limit: VALUE_MAX });
                }
                (self.families.whose)(family, key).and_then(session_ok).map(str::to_owned).ok_or_else(|| Refusal::Invalid { what: "a key", why: format!("it names no session of family {family:?}") })
            })
            .collect()
    }

    /// A commit made: counted, and its families' waits told.
    fn committed(&self, tx: &Tx, rev: Rev) {
        if tx.writes.is_empty() {
            return;
        }
        self.head.fetch_max(rev, Ordering::AcqRel);
        {
            let mut newest = self.newest.lock();
            for w in &tx.writes {
                let family = match w {
                    Write::Put { family, .. } | Write::Delete { family, .. } => family,
                };
                newest.insert(family.clone(), rev);
            }
        }
        self.changed.notify();
    }

    async fn commit_in(&self, tx: Tx, owner: Option<Box<dyn Send + Sync>>) -> Result<Rev, Refusal> {
        let sessions = self.sessions_of(&tx)?;
        let store = self.source.store()?;
        let shape = Tx { conds: Vec::new(), writes: tx.writes.iter().map(|w| match w {
            Write::Put { family, key, .. } | Write::Delete { family, key } => Write::Delete { family: family.clone(), key: key.clone() },
        }).collect() };
        let rev = store.inbox_commit(tx, sessions, owner).await.map_err(|e| unavailable(&e))??;
        self.committed(&shape, rev);
        Ok(rev)
    }

    /// The inbox's rows in its file of its own from before brought into the store, before anything is answered; then
    /// the file is set aside.
    async fn ready(&self) -> Result<(), Refusal> {
        if self.legacy.lock().is_none() {
            return Ok(());
        }
        let _bringing = self.bringing.lock().await;
        let Some((legacy, path)) = self.legacy.lock().clone() else { return Ok(()) };
        let store = self.source.store()?;
        // Its numbers go on past the file's: a take after it is numbered past every take before.
        let top = legacy.head().await?;
        store.inbox_floor(top).await.map_err(|e| unavailable(&e))?;
        let mut moved = 0usize;
        for family in self.families.all {
            let mut after: Option<Vec<u8>> = None;
            loop {
                let page = legacy.scan(family, Scan { prefix: Vec::new(), after: after.clone(), limit: SCAN_MAX, at: Consistency::Linearizable }).await?;
                let Some(last) = page.rows.last().map(|row| row.key.clone()) else { break };
                for chunk in page.rows.chunks(TX_MAX) {
                    let writes = chunk.iter().map(|row| {
                        let value = crate::harness::inbox::with_receipt_sequence(family,&row.value,row.rev).map_err(|e| unavailable(&e))?;
                        Ok(Write::Put { family: (*family).to_owned(), key: row.key.clone(), value })
                    }).collect::<Result<Vec<_>,Refusal>>()?;
                    let tx = Tx { conds: Vec::new(), writes };
                    match self.sessions_of(&tx) {
                        Ok(sessions) => {
                            store.inbox_commit(tx, sessions, None).await.map_err(|e| unavailable(&e))??;
                            moved = moved.saturating_add(chunk.len());
                        }
                        Err(e) => tracing::warn!(target: "kiln.sends", event = "inbox_row_unowned", family = %family, error = %e, "rows of the inbox's file of its own that name no session: left there"),
                    }
                }
                if !page.more {
                    break;
                }
                after = Some(last);
            }
        }
        *self.legacy.lock() = None;
        let aside = path.with_extension("db.brought");
        crate::rt::blocking(move || -> std::io::Result<()> {
            // Its thread finishes what it was given and lets go of the file first.
            drop(legacy);
            for suffix in ["", "-wal", "-shm"] {
                let mut from = path.clone().into_os_string();
                from.push(suffix);
                let mut to = aside.clone().into_os_string();
                to.push(suffix);
                match std::fs::rename(&from, &to) {
                    Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e),
                    _ => {}
                }
            }
            Ok(())
        })
        .await
        .map_err(|e| unavailable(&e))?
        .map_err(|e| Refusal::Unavailable(format!("the inbox's file of its own couldn't be set aside once its rows were brought over: {e}")))?;
        tracing::info!(target: "kiln.sends", event = "inbox_brought_over", rows = moved, "each session's inbox is in its runner's conversation store now");
        Ok(())
    }
}

impl Records for InboxRecords {
    fn get<'a>(&'a self, family: &'a str, key: &'a [u8], _at: Consistency) -> BoxFuture<'a, Result<Option<Row>, Refusal>> {
        Box::pin(async move {
            self.ready().await?;
            let store = self.source.store()?;
            let row = store.inbox_get(family.to_owned(), key.to_vec()).await.map_err(|e| unavailable(&e))?;
            Ok(row.map(|(value, rev)| Row { key: key.to_vec(), value, rev }))
        })
    }

    fn scan<'a>(&'a self, family: &'a str, scan: Scan) -> BoxFuture<'a, Result<Page, Refusal>> {
        Box::pin(async move {
            if scan.limit == 0 || scan.limit > SCAN_MAX {
                return Err(Refusal::Invalid { what: "a scan's limit", why: format!("{}: 1 to {SCAN_MAX}", scan.limit) });
            }
            self.ready().await?;
            let store = self.source.store()?;
            let (rows, more) = store.inbox_scan(family.to_owned(), scan.prefix, scan.after, scan.limit).await.map_err(|e| unavailable(&e))?;
            Ok(Page { rows: rows.into_iter().map(|(key, value, rev)| Row { key, value, rev }).collect(), more })
        })
    }

    fn commit<'a>(&'a self, tx: Tx) -> BoxFuture<'a, Result<Rev, Refusal>> {
        Box::pin(async move {
            self.ready().await?;
            self.commit_in(tx, None).await
        })
    }

    fn commit_owned<'a>(&'a self, tx: Tx, owner: Box<dyn Send + Sync>) -> BoxFuture<'a, Result<Rev, Refusal>> {
        Box::pin(async move {
            if let Err(e) = self.ready().await {
                drop((tx, owner));
                return Err(e);
            }
            self.commit_in(tx, Some(owner)).await
        })
    }

    fn changes<'a>(&'a self, _family: &'a str, _after: Rev, _limit: usize) -> BoxFuture<'a, Result<Changes, Refusal>> {
        Box::pin(async move { Err(Refusal::Unavailable("the inbox's changes go with its conversation store's feed".into())) })
    }

    fn wait<'a>(&'a self, family: &'a str, after: Rev) -> BoxFuture<'a, Result<Rev, Refusal>> {
        Box::pin(async move {
            self.ready().await?;
            loop {
                let listen = self.changed.listen();
                if let Some(rev) = self.newest.lock().get(family).copied().filter(|rev| *rev > after) {
                    return Ok(rev);
                }
                listen.await;
            }
        })
    }

    fn head(&self) -> BoxFuture<'_, Result<Rev, Refusal>> {
        Box::pin(async move {
            self.ready().await?;
            Ok(self.head.load(Ordering::Acquire))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kiln_seams::records::{Cond, SimRecords};

    fn families() -> Families {
        crate::harness::inbox::FAMILIES
    }

    fn put(family: &str, key: &str, value: &str) -> Write {
        Write::Put { family: family.into(), key: key.as_bytes().to_vec(), value: value.as_bytes().to_vec() }
    }

    #[test]
    fn each_sessions_inbox_is_kept_in_its_runners_conversation_store() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (_dir, store) = super::super::store::scratch()?;
            let records = InboxRecords::new(Source::Store(store.clone()), families(), None);
            let rev = records.commit(Tx { conds: vec![Cond::Absent { family: "inbox".into(), key: b"s1/m1".to_vec() }], writes: vec![put("inbox", "s1/m1", "taken"), put("inbox-heads", "s1", "head")] }).await?;
            let row = records.get("inbox", b"s1/m1", Consistency::Linearizable).await?.ok_or_else(|| anyhow::anyhow!("not kept"))?;
            anyhow::ensure!(row.rev == rev && row.value == b"taken");
            anyhow::ensure!(records.get("inbox-heads", b"s1", Consistency::Linearizable).await?.is_some_and(|h| h.rev == rev), "one number for the commit's every write");
            // Taken again: its condition fails, naming where it is.
            let again = records.commit(Tx { conds: vec![Cond::Absent { family: "inbox".into(), key: b"s1/m1".to_vec() }], writes: vec![put("inbox", "s1/m1", "again")] }).await;
            anyhow::ensure!(matches!(&again, Err(Refusal::Conflict { at: Some(at), .. }) if *at == rev), "{again:?}");
            // A delete is gone to reads, and a key no session names is refused.
            records.commit(Tx { conds: vec![Cond::At { family: "inbox".into(), key: b"s1/m1".to_vec(), rev }], writes: vec![Write::Delete { family: "inbox".into(), key: b"s1/m1".to_vec() }] }).await?;
            anyhow::ensure!(records.get("inbox", b"s1/m1", Consistency::Linearizable).await?.is_none());
            anyhow::ensure!(matches!(records.commit(Tx { conds: Vec::new(), writes: vec![put("not-the-inbox", "x", "y")] }).await, Err(Refusal::Invalid { .. })));
            // A scan names its session, in key order, past where it was.
            for m in ["a", "b", "c"] {
                records.commit(Tx { conds: Vec::new(), writes: vec![put("inbox", &format!("s2/{m}"), m)] }).await?;
            }
            records.commit(Tx { conds: Vec::new(), writes: vec![put("inbox", "s20/a", "another session")] }).await?;
            let page = records.scan("inbox", Scan { prefix: b"s2/".to_vec(), after: Some(b"s2/a".to_vec()), limit: 1, at: Consistency::Linearizable }).await?;
            anyhow::ensure!(page.rows.iter().map(|r| r.key.clone()).collect::<Vec<_>>() == vec![b"s2/b".to_vec()] && page.more, "{page:?}");
            let rest = records.scan("inbox", Scan { prefix: b"s2/".to_vec(), after: Some(b"s2/b".to_vec()), limit: 10, at: Consistency::Linearizable }).await?;
            anyhow::ensure!(rest.rows.len() == 1 && !rest.more, "{rest:?}");
            Ok(())
        })
    }

    #[test]
    fn what_was_taken_stays_taken_when_the_store_opens_again() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let dir = tempfile::tempdir()?;
            let path = dir.path().join(super::super::FILE);
            {
                let store = Store::start(Box::new(super::super::turso::Turso::open(&path)?), super::super::store::Upkeep::default())?;
                let records = InboxRecords::new(Source::Store(store.clone()), families(), None);
                records.commit(Tx { conds: Vec::new(), writes: vec![put("inbox", "s1/m1", "taken")] }).await?;
                store.close()?;
            }
            let store = Store::start(Box::new(super::super::turso::Turso::open(&path)?), super::super::store::Upkeep::default())?;
            let records = InboxRecords::new(Source::Store(store), families(), None);
            anyhow::ensure!(records.get("inbox", b"s1/m1", Consistency::Linearizable).await?.is_some(), "a take is kept across the store's restart");
            Ok(())
        })
    }

    #[test]
    fn the_inboxs_file_of_its_own_is_brought_over_and_its_numbers_keep_rising() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (dir, store) = super::super::store::scratch()?;
            let legacy = Arc::new(SimRecords::new());
            let mut last = 0;
            for n in 0..50 {
                last = legacy.commit(Tx { conds: Vec::new(), writes: vec![put("inbox", &format!("s1/m{n:02}"), "taken"), put("inbox-heads", "s1", "head")] }).await?;
            }
            let path = dir.path().join(crate::records::FILE);
            std::fs::write(&path, b"stand-in")?;
            let records = InboxRecords::new(Source::Store(store), families(), Some((Arc::clone(&legacy) as Arc<dyn Records>, path.clone())));
            let row = records.get("inbox", b"s1/m49", Consistency::Linearizable).await?.ok_or_else(|| anyhow::anyhow!("not brought over"))?;
            anyhow::ensure!(row.rev > last, "{} past the file's {last}", row.rev);
            let next = records.commit(Tx { conds: Vec::new(), writes: vec![put("inbox", "s1/m50", "taken")] }).await?;
            anyhow::ensure!(next > row.rev);
            anyhow::ensure!(!path.exists() && path.with_extension("db.brought").exists(), "the file is set aside once brought over");
            Ok(())
        })
    }

    /// Replicated across with the conversation (Ada, 2026-11-20): a runner's inbox rows go to the server's store with its
    /// feed, a delete as a delete, an inbox commit's rows together; a taker from before isn't sent them, and the feed
    /// reads on past them.
    #[test]
    fn a_runners_inbox_rows_go_to_the_server_with_its_feed_and_their_deletes_too() -> anyhow::Result<()> {
        use super::super::rows::Row as Kept;
        crate::rt::block_on(async {
            let (_a, runner) = super::super::store::scratch()?;
            let (_b, server) = super::super::store::scratch()?;
            runner.put(vec![Kept::Session { session: "s1".into(), info: Some("{}".into()), meta: None, state: None, changes: None, epoch: Some(1), open: true, at: 1 }]);
            let records = InboxRecords::new(Source::Store(runner.clone()), families(), None);
            records.commit(Tx { conds: Vec::new(), writes: vec![put("inbox", "s1/m1", "taken"), put("inbox-heads", "s1", "head")] }).await?;
            records.commit(Tx { conds: Vec::new(), writes: vec![put("inbox", "s1/m2", "taken")] }).await?;
            records.commit(Tx { conds: Vec::new(), writes: vec![Write::Delete { family: "inbox".into(), key: b"s1/m2".to_vec() }] }).await?;
            let before = runner.changes_as(0, 1 << 20, false, false)?;
            anyhow::ensure!(!before.rows.iter().any(|c| matches!(c.row, Kept::Inbox(_))), "a taker from before isn't sent them");
            let batch = runner.changes_as(0, 1 << 20, false, true)?;
            anyhow::ensure!(batch.upto == before.upto, "the feed reads past them either way: {} and {}", batch.upto, before.upto);
            let fed = batch.rows.iter().filter(|c| matches!(c.row, Kept::Inbox(_))).count();
            anyhow::ensure!(fed == 3, "the commit's two rows together, and the delete: {fed}");
            server.take("runner", Some("mac"), 0, batch)?;
            let copy = InboxRecords::new(Source::Store(server), families(), None);
            anyhow::ensure!(copy.get("inbox", b"s1/m1", Consistency::Linearizable).await?.is_some_and(|r| r.value == b"taken"));
            anyhow::ensure!(copy.get("inbox-heads", b"s1", Consistency::Linearizable).await?.is_some());
            anyhow::ensure!(copy.get("inbox", b"s1/m2", Consistency::Linearizable).await?.is_none(), "a delete goes with the feed");
            Ok(())
        })
    }

    #[test]
    fn a_held_store_not_given_yet_refuses_and_a_given_one_serves() -> anyhow::Result<()> {
        crate::rt::block_on(async {
            let (tx, _rx) = async_channel::unbounded();
            let (_sink, held) = super::super::store::tee_held(crate::harness::session::To::Channel(tx), None);
            let records = InboxRecords::new(Source::Held(held.clone()), families(), None);
            anyhow::ensure!(matches!(records.get("inbox", b"s1/m1", Consistency::Linearizable).await, Err(Refusal::Unavailable(_))));
            let (_dir, store) = super::super::store::scratch()?;
            held.give(store);
            records.commit(Tx { conds: Vec::new(), writes: vec![put("inbox", "s1/m1", "taken")] }).await?;
            anyhow::ensure!(records.get("inbox", b"s1/m1", Consistency::Linearizable).await?.is_some());
            Ok(())
        })
    }
}
