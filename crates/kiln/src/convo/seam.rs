//! kiln's conversation store behind kiln-seams' Conversations seam (docs/seams.md; CP-T1.1): a [`Store`] on either
//! engine ([`super::Kind`]: Turso, the control, or Amphora's), held to the seam's conformance suite and run through
//! its load scenario, each engine the same way.

use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kiln_seams::conversations::{Changed, Conversations, Entry, Error, Page};
use parking_lot::Mutex;

use super::rows::{self, Row};
use super::store::{Refusal, Store};
use crate::harness::core::Update;
use crate::harness::model::{Body, Event};

/// CP-T1.1 A/B (lane vendor-amphora-11): how an append knows a conversation's session row is written (a session's
/// row goes before its first event, as kiln's harness writes it). The load scenario's ladder measures both: append
/// latency and the memory held a conversation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Opened {
    /// The control: a set in memory of the conversations this handle gave their row, let go of as it closes: an entry
    /// a conversation it appended to, for the handle's life.
    Set,
    /// The store asked, beside its writer, whether the row is there, once for each conversation of each append:
    /// nothing held a conversation, a read more an append.
    Ask,
}

/// Which [`Opened`] a handle keeps: `set` (the control, and unset) or `ask`.
pub const OPENED_VAR: &str = "KILN_CONVO_OPENED";

impl Opened {
    /// As [`OPENED_VAR`] says: unset, the control; anything but `set` or `ask` refused by name.
    pub fn configured() -> anyhow::Result<Self> {
        match std::env::var(OPENED_VAR) {
            Err(std::env::VarError::NotPresent) => Ok(Self::Set),
            Err(e) => Err(anyhow::anyhow!("{OPENED_VAR}: {e}")),
            Ok(v) => Self::named(&v),
        }
    }

    /// By its name in [`OPENED_VAR`].
    pub fn named(v: &str) -> anyhow::Result<Self> {
        match v.trim() {
            "set" => Ok(Self::Set),
            "ask" => Ok(Self::Ask),
            other => anyhow::bail!("{OPENED_VAR} is `{other}`: it takes `set` or `ask`"),
        }
    }
}

/// [`Opened::Set`]'s conversations, and the bytes their names take.
#[derive(Default)]
struct OpenedSet {
    set: HashSet<String>,
    name_bytes: usize,
}

impl OpenedSet {
    /// What it holds, at least: a name and a control byte for each entry its table has room for (std's hash table's
    /// layout) and the names' own bytes.
    fn bytes(&self) -> usize {
        self.set.capacity().saturating_mul(std::mem::size_of::<String>().saturating_add(1)).saturating_add(self.name_bytes)
    }

    fn insert(&mut self, name: &str) -> bool {
        if self.set.contains(name) {
            return false;
        }
        self.name_bytes = self.name_bytes.saturating_add(name.len());
        self.set.insert(name.to_owned())
    }

    fn remove(&mut self, name: &str) {
        if self.set.remove(name) {
            self.name_bytes = self.name_bytes.saturating_sub(name.len());
        }
    }

    fn clear(&mut self) {
        self.set = HashSet::new();
        self.name_bytes = 0;
    }
}

/// A [`Store`] as the seam's Conversations: an entry is an assistant's event of its session.
pub struct StoreConversations {
    store: Store,
    /// How an append knows a session's row is written ([`Opened`]).
    by: Opened,
    /// [`Opened::Set`]'s sessions given their row, let go of as the handle closes (seam review 8).
    opened: Mutex<OpenedSet>,
    /// The most bytes `opened` held, for the load scenario's memory a conversation; shared by the handles a fixture
    /// opens.
    held_most: Arc<AtomicUsize>,
    closed: AtomicBool,
    /// Told as the handle is let go of: a wait under way hears it and is refused Closed ([`Conversations::wait`]).
    closing: crate::rt::Shutdown,
}

impl StoreConversations {
    /// A handle keeping what [`OPENED_VAR`] says (the control where it can't be read: it's said).
    pub fn new(store: Store) -> Self {
        let by = Opened::configured().unwrap_or_else(|e| {
            tracing::warn!(error = %format!("{e:#}"), "the conversation seam keeps its opened sessions in memory, the control");
            Opened::Set
        });
        Self::with(store, by, Arc::default())
    }

    /// A handle keeping `by`, the most its set held told to `held_most`.
    pub fn with(store: Store, by: Opened, held_most: Arc<AtomicUsize>) -> Self {
        Self { store, by, opened: Mutex::new(OpenedSet::default()), held_most, closed: AtomicBool::new(false), closing: crate::rt::Shutdown::new() }
    }

    /// The conversations of `entries` this append gives their row: for [`Opened::Set`], those not in its set (put in
    /// it now); for [`Opened::Ask`], those whose row the store says isn't there, at its last commit.
    fn unopened(&self, entries: &[Entry]) -> Result<HashSet<String>, Error> {
        let mut new = HashSet::new();
        match self.by {
            Opened::Set => {
                let mut opened = self.opened.lock();
                for e in entries {
                    if opened.insert(&e.conversation) {
                        new.insert(e.conversation.clone());
                    }
                }
                self.held_most.fetch_max(opened.bytes(), Ordering::Relaxed);
            }
            Opened::Ask => {
                let mut asked = HashSet::new();
                for e in entries {
                    if asked.insert(e.conversation.as_str()) && self.store.restorable_committed(&e.conversation, 0).map_err(|x| self.failed(x))?.is_none() {
                        new.insert(e.conversation.clone());
                    }
                }
            }
        }
        Ok(new)
    }

    fn live(&self) -> Result<(), Error> {
        if self.closed.load(Ordering::Acquire) { Err(Error::Closed) } else { Ok(()) }
    }

    /// The store's error as the seam's, Closed when the handle was let go of meanwhile or its writer has ended: what
    /// was under way as it closed (its writer gone with it) is refused Closed, as what's asked after is, and never
    /// written once refused.
    fn failed(&self, e: anyhow::Error) -> Error {
        error_of(self.closed.load(Ordering::Acquire) || self.store.stopped(), e)
    }
}

/// The store's error as the seam's (seam review 9): a handle closed, or the store's writer gone, is Closed; past one of
/// its bounds ([`Refusal`]) is Refused with its reason; anything else (the disk, the engine) Failed in its words.
fn error_of(closed: bool, e: anyhow::Error) -> Error {
    match e.downcast_ref::<Refusal>() {
        _ if closed => Error::Closed,
        Some(Refusal::Stopped | Refusal::StoppedBeforeOutcome) => Error::Closed,
        Some(r @ (Refusal::NoAnswer | Refusal::NoOutcome)) => Error::Refused(r.to_string()),
        None => Error::Failed(format!("{e:#}")),
    }
}

fn entry_of(r: &rows::EventRow) -> Entry {
    Entry { conversation: r.session.clone(), id: r.id.clone(), seq: r.seq, text: r.text.clone() }
}

impl Conversations for StoreConversations {
    fn append(&self, entries: &[Entry]) -> Result<(), Error> {
        self.live()?;
        let now = crate::util::now_ms();
        // Sessions given their row by this append: theirs to give again if it isn't written.
        let new = self.unopened(entries)?;
        let mut out = Vec::with_capacity(entries.len().saturating_add(new.len()));
        let mut given = HashSet::new();
        for e in entries {
            // Its row before its first event.
            if new.contains(&e.conversation) && given.insert(e.conversation.as_str()) {
                out.push(Row::Session { session: e.conversation.clone(), info: None, meta: None, state: None, changes: None, epoch: None, open: true, at: now });
            }
            let event = Event { id: e.id.clone(), seq: e.seq, at: i64::try_from(e.seq).unwrap_or(i64::MAX), parent: None, by: None, body: Body::Assistant { text: e.text.clone(), streaming: false } };
            out.extend(rows::of(&e.conversation, &Update::Events { events: vec![event] }, now));
        }
        // Ok only once the commit these rows went in was written: the writer's own outcome (CP-T1.1).
        self.store.put_committed(out).map_err(|e| {
            if self.by == Opened::Set {
                let mut opened = self.opened.lock();
                for s in &new {
                    opened.remove(s);
                }
            }
            self.failed(e)
        })
    }

    fn history(&self, conversation: &str, before: Option<u64>, limit: usize) -> Result<Vec<Entry>, Error> {
        self.live()?;
        // Beside the writer, at the last commit: what an append was answered Ok for is in it (CP-T1.1).
        Ok(self.store.events_committed(conversation, before, limit).map_err(|e| self.failed(e))?.iter().map(entry_of).collect())
    }

    fn changes(&self, from: u64, max_bytes: usize) -> Result<Page, Error> {
        self.live()?;
        // The store's feed hands on its sessions' rows too: past a page of them alone, on to the next.
        let mut at = from;
        loop {
            // Beside the writer, the page and where it reads through of one commit: a follower's pages never wait
            // behind appends, nor hold them up (CP-T1.1).
            let batch = self.store.changes_committed(at, max_bytes).map_err(|e| self.failed(e))?;
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
        // What's committed, as the writer said it: no turn of the writer's for it (a follower asks it every page).
        Ok(self.store.committed_top())
    }

    fn wait(&self, after: u64) -> Result<u64, Error> {
        // Watching before looking: a commit between the look and the wait leaves its word, so none is missed. The
        // store's writer sets the top it committed, then tells its watchers of each commit (`Store::watch`,
        // `Store::committed_top`): a reader woken asks nothing of the writer. The handle let go of meanwhile ends it,
        // as the store's writer ending does (it tells its watchers as it ends: `Store::stopped`).
        let commits = self.store.watch();
        loop {
            self.live()?;
            if self.store.stopped() {
                return Err(Error::Closed);
            }
            let top = self.store.committed_top();
            if top > after {
                return Ok(top);
            }
            let _ = futures::executor::block_on(futures::future::select(std::pin::pin!(commits.notified()), std::pin::pin!(self.closing.cancelled())));
        }
    }

    fn close(&self) -> Result<(), Error> {
        if self.closed.swap(true, Ordering::AcqRel) {
            return Ok(());
        }
        self.closing.cancel();
        self.opened.lock().clear();
        // A writer that ended on its own has let go of its files already: nothing more to let go of.
        match self.store.close() {
            Err(e) if !matches!(e.downcast_ref::<Refusal>(), Some(Refusal::Stopped)) => Err(error_of(false, e)),
            _ => Ok(()),
        }
    }
}

/// A store on `kind`'s engine in a folder of its own, opened as often as a case asks.
#[cfg(test)]
pub struct Fixture {
    pub kind: super::Kind,
    /// Its folder (None: none could be made, and opening says so).
    pub dir: Option<tempfile::TempDir>,
    /// While set, the engine's writes fail (CONVO-7): its engine is wrapped in a [`super::store::FailsWrites`].
    pub failing: Arc<AtomicBool>,
    /// The A/B arms its handles and stores run ([`Opened`], [`super::store::Readers`]), or why they couldn't be read.
    pub arms: Result<(Opened, super::store::Readers), String>,
    /// The most bytes [`Opened::Set`]'s set held in any handle it opened.
    pub held_most: Arc<AtomicUsize>,
}

#[cfg(test)]
impl Fixture {
    /// On the arms [`OPENED_VAR`] and [`super::store::READERS_VAR`] say (the controls, unset).
    pub fn new(kind: super::Kind, dir: Option<tempfile::TempDir>) -> Self {
        let arms = Opened::configured().and_then(|o| super::store::Readers::configured().map(|r| (o, r))).map_err(|e| format!("{e:#}"));
        Self { kind, dir, failing: Arc::new(AtomicBool::new(false)), arms, held_most: Arc::default() }
    }

    /// On the arms given.
    pub fn arm(kind: super::Kind, dir: Option<tempfile::TempDir>, opened: Opened, readers: super::store::Readers) -> Self {
        Self { arms: Ok((opened, readers)), ..Self::new(kind, dir) }
    }
}

#[cfg(test)]
impl kiln_seams::conversations::ConversationsFixture for Fixture {
    fn open(&self) -> Result<Arc<dyn Conversations>, String> {
        let dir = self.dir.as_ref().ok_or_else(|| "no folder for the store".to_owned())?;
        let (opened, readers) = self.arms.clone()?;
        let inner = self.kind.open(&dir.path().join(super::FILE)).map_err(|e| format!("{e:#}"))?;
        let engine = Box::new(super::store::FailsWrites { inner, failing: Arc::clone(&self.failing) });
        let store = Store::start_with(engine, super::store::Upkeep::default(), readers).map_err(|e| format!("{e:#}"))?;
        Ok(Arc::new(StoreConversations::with(store, opened, Arc::clone(&self.held_most))))
    }

    fn fail_writes(&self, failing: bool) {
        self.failing.store(failing, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::super::store::Readers;
    use super::*;
    use kiln_seams::conversations::{Load, cases};

    /// CP-T1.1: the conversation store passes every clause of the Conversations seam on each engine, Turso and
    /// Amphora's (the feed's exactly-once-in-order model test, CONVO-3 and CONVO-4, among them).
    #[test]
    fn cp_t1_1_the_store_passes_the_conversations_suite_on_each_engine() -> anyhow::Result<()> {
        for kind in super::super::Kind::ALL {
            let report = crate::rt::block_on(kiln_seams::conformance::run("Conversations", &cases::<Fixture>(), || {
                let dir = tempfile::tempdir().ok();
                async move { Fixture::new(kind, dir) }
            }));
            report.into_result().map_err(|e| anyhow::anyhow!("{kind:?}: {e}"))?;
        }
        Ok(())
    }

    /// CP-T1.1 (seam review 1): an append whose rows the engine couldn't write answers an error, never Ok, and isn't
    /// in what's read after; once the engine writes again, the same append goes in whole. Without the writer's outcome
    /// carried back, the failed batch was logged and the append answered Ok.
    #[test]
    fn cp_t1_1_an_append_that_isnt_written_answers_an_error() -> anyhow::Result<()> {
        for kind in super::super::Kind::ALL {
            let dir = tempfile::tempdir()?;
            let failing = Arc::new(AtomicBool::new(false));
            let inner = kind.open(&dir.path().join(super::super::FILE))?;
            let engine = Box::new(super::super::store::FailsWrites { inner, failing: Arc::clone(&failing) });
            let c = StoreConversations::new(Store::start(engine, super::super::store::Upkeep::default())?);
            let first = Entry { conversation: "a".into(), id: "m1".into(), seq: 1, text: "kept".into() };
            c.append(std::slice::from_ref(&first)).map_err(|e| anyhow::anyhow!("{kind:?}: {e}"))?;
            failing.store(true, Ordering::Release);
            let lost = [Entry { conversation: "b".into(), id: "m1".into(), seq: 1, text: "not written".into() }];
            let answer = c.append(&lost);
            anyhow::ensure!(matches!(answer, Err(Error::Failed(_))), "{kind:?}: an append that wasn't written answered {answer:?}");
            failing.store(false, Ordering::Release);
            anyhow::ensure!(c.history("b", None, 10).map_err(|e| anyhow::anyhow!("{e}"))?.is_empty(), "{kind:?}: what wasn't written was read");
            // Its session's row goes again with it: the retry is read back whole.
            c.append(&lost).map_err(|e| anyhow::anyhow!("{kind:?}: the retry: {e}"))?;
            let b = c.history("b", None, 10).map_err(|e| anyhow::anyhow!("{e}"))?;
            anyhow::ensure!(b == lost.to_vec(), "{kind:?}: the retry read back as {b:?}");
            c.close().map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        Ok(())
    }

    /// CP-T1.1 (lane vendor-amphora-9's leftover): a store whose writer ends on its own (here, let go of through
    /// another handle to it, not this one) wakes a wait under way, refused Closed, never left hanging on a commit that
    /// can't come; what's asked after is Closed too, and closing this handle then is Ok (nothing more to let go of).
    /// Without the writer telling its watchers as it ends, the wait hung.
    #[test]
    fn cp_t1_1_a_writer_that_ends_on_its_own_ends_every_wait_closed() -> anyhow::Result<()> {
        for kind in super::super::Kind::ALL {
            let dir = tempfile::tempdir()?;
            let store = Store::start(kind.open(&dir.path().join(super::super::FILE))?, super::super::store::Upkeep::default())?;
            let c = Arc::new(StoreConversations::new(store.clone()));
            c.append(&[Entry { conversation: "a".into(), id: "m1".into(), seq: 1, text: "first".into() }]).map_err(|e| anyhow::anyhow!("{kind:?}: {e}"))?;
            let top = c.top().map_err(|e| anyhow::anyhow!("{kind:?}: {e}"))?;
            let (tx, rx) = std::sync::mpsc::channel();
            let waiter = {
                let c = Arc::clone(&c);
                std::thread::spawn(move || {
                    let _ = tx.send(c.wait(top));
                })
            };
            store.close()?;
            // A guard against hanging only, not a measure: the store's own bound on a wait for its writer.
            let woke = rx.recv_timeout(super::super::store::READ_WAIT);
            anyhow::ensure!(matches!(woke, Ok(Err(Error::Closed))), "{kind:?}: a wait as the store's writer ended answered {woke:?}");
            let _ = waiter.join();
            let after = c.append(&[Entry { conversation: "a".into(), id: "m2".into(), seq: 2, text: "after".into() }]);
            anyhow::ensure!(matches!(after, Err(Error::Closed)), "{kind:?}: an append after the writer ended answered {after:?}");
            c.close().map_err(|e| anyhow::anyhow!("{kind:?}: closing after the writer ended: {e}"))?;
        }
        Ok(())
    }

    /// CP-T1.1 (seam review 9): the store's errors as the seam's: past one of its bounds Refused, saying which; its
    /// writer gone, or the handle closed, Closed; the disk's or the engine's fault Failed, in their words.
    #[test]
    fn cp_t1_1_the_stores_refusals_are_refused_its_faults_failed() {
        let refused = error_of(false, Refusal::NoAnswer.into());
        assert!(matches!(&refused, Error::Refused(why) if why == &Refusal::NoAnswer.to_string()), "{refused:?}");
        let refused = error_of(false, Refusal::NoOutcome.into());
        assert!(matches!(&refused, Error::Refused(why) if why == &Refusal::NoOutcome.to_string()), "{refused:?}");
        for gone in [Refusal::Stopped, Refusal::StoppedBeforeOutcome] {
            let closed = error_of(false, gone.into());
            assert!(matches!(closed, Error::Closed), "{gone:?}: {closed:?}");
        }
        let failed = error_of(false, anyhow::anyhow!("disk I/O error"));
        assert!(matches!(&failed, Error::Failed(why) if why == "disk I/O error"), "{failed:?}");
        let closed = error_of(true, anyhow::anyhow!("disk I/O error"));
        assert!(matches!(closed, Error::Closed), "{closed:?}");
    }

    /// CP-T1.1 (seam review 8): the sessions a handle gave their row are let go of as it closes, not kept for the
    /// process's life.
    #[test]
    fn cp_t1_1_a_closed_handle_lets_go_of_the_sessions_it_opened() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let kind = super::super::Kind::Turso;
        let c = StoreConversations::with(Store::start(kind.open(&dir.path().join(super::super::FILE))?, super::super::store::Upkeep::default())?, Opened::Set, Arc::default());
        let batch: Vec<Entry> = (1..=8u64).map(|i| Entry { conversation: format!("c{i}"), id: "m1".into(), seq: 1, text: "words".into() }).collect();
        c.append(&batch).map_err(|e| anyhow::anyhow!("{e}"))?;
        anyhow::ensure!(c.opened.lock().set.len() == batch.len(), "{} sessions held for {} conversations", c.opened.lock().set.len(), batch.len());
        c.close().map_err(|e| anyhow::anyhow!("{e}"))?;
        let (left, bytes) = {
            let held = c.opened.lock();
            (held.set.len(), held.bytes())
        };
        anyhow::ensure!(left == 0 && bytes == 0, "{left} sessions ({bytes} bytes) still held after close");
        Ok(())
    }

    /// CP-T1.1 A/B (lane vendor-amphora-11): asking the store (`Opened::Ask`) holds nothing a conversation, and gives
    /// each session its row once, before its first event: a conversation appended to again, or by another handle (here
    /// the store opened again, where the set, empty again, gives it a second), gets no second row, and a batch naming
    /// one conversation twice gives it one. The feed hands on a session's row at the revision it was last written, so
    /// a row written again comes after its events there; on each engine.
    #[test]
    fn cp_t1_1_asking_the_store_gives_each_session_its_row_once_and_holds_nothing() -> anyhow::Result<()> {
        for kind in super::super::Kind::ALL {
            let f = Fixture::arm(kind, Some(tempfile::tempdir()?), Opened::Ask, Readers::One);
            let e = |c: &str, i: u64| Entry { conversation: c.into(), id: format!("m{i}"), seq: i, text: "words".into() };
            let c = kiln_seams::conversations::ConversationsFixture::open(&f).map_err(|e| anyhow::anyhow!("{kind:?}: {e}"))?;
            c.append(&[e("a", 1), e("a", 2), e("b", 1)]).map_err(|e| anyhow::anyhow!("{kind:?}: {e}"))?;
            c.append(&[e("a", 3)]).map_err(|e| anyhow::anyhow!("{kind:?}: {e}"))?;
            c.close().map_err(|e| anyhow::anyhow!("{e}"))?;
            let c = kiln_seams::conversations::ConversationsFixture::open(&f).map_err(|e| anyhow::anyhow!("{kind:?}: opened again: {e}"))?;
            c.append(&[e("b", 2)]).map_err(|e| anyhow::anyhow!("{kind:?}: {e}"))?;
            let top = c.top().map_err(|e| anyhow::anyhow!("{e}"))?;
            c.close().map_err(|e| anyhow::anyhow!("{e}"))?;
            anyhow::ensure!(f.held_most.load(Ordering::Relaxed) == 0, "{kind:?}: asking held {} bytes", f.held_most.load(Ordering::Relaxed));
            // The store's own feed, sessions' rows and all: one row a session, before its first event.
            let dir = f.dir.as_ref().ok_or_else(|| anyhow::anyhow!("no folder"))?;
            let store = Store::start(kind.open(&dir.path().join(super::super::FILE))?, super::super::store::Upkeep::default())?;
            let batch = store.changes(0, 1 << 20)?;
            store.close()?;
            let (mut rowed, mut evented) = (HashSet::new(), HashSet::new());
            for ch in &batch.rows {
                match &ch.row {
                    Row::Session { session, .. } => {
                        anyhow::ensure!(!evented.contains(session), "{kind:?}: {session}'s row was written again, after its events (top {top})");
                        rowed.insert(session.clone());
                    }
                    Row::Event(ev) => {
                        anyhow::ensure!(rowed.contains(&ev.session), "{kind:?}: {}'s event came before its row", ev.session);
                        evented.insert(ev.session.clone());
                    }
                    _ => {}
                }
            }
            anyhow::ensure!(rowed.len() == 2 && evented.len() == 2, "{kind:?}: rows for {rowed:?}, events for {evented:?}");
        }
        Ok(())
    }

    /// CP-T1.1 A/B: the opened switch by name, the control unset; anything else refused, naming what it takes.
    #[test]
    fn cp_t1_1_the_opened_switch_takes_set_or_ask() {
        assert_eq!(Opened::named("set").ok(), Some(Opened::Set));
        assert_eq!(Opened::named(" ask\n").ok(), Some(Opened::Ask));
        let refused = Opened::named("cache").err().map(|e| e.to_string()).unwrap_or_default();
        assert!(refused.contains("`set` or `ask`"), "{refused}");
    }

    /// The folder and engine a crashing child writes in ([`cp_t1_1_every_acknowledged_append_stays_through_a_crash`]).
    const CRASH_DIR: &str = "KILN_CONVO_CRASH_DIR";
    const CRASH_ENGINE: &str = "KILN_CONVO_CRASH_ENGINE";
    /// The test's size: the child is killed once it has said this many appends were acknowledged, more under way, and
    /// it appends no more than `CRASH_MOST` were it never killed.
    const CRASH_AFTER: usize = 32;
    const CRASH_MOST: u64 = 100_000;

    /// The crashing child: appends to one conversation, an entry an append, saying each once it's acknowledged, until
    /// it's killed.
    fn crash_child(dir: &std::path::Path) -> anyhow::Result<()> {
        use std::io::Write;
        let kind = super::super::Kind::named(&std::env::var(CRASH_ENGINE)?)?;
        let c = StoreConversations::new(Store::start(kind.open(&dir.join(super::super::FILE))?, super::super::store::Upkeep::default())?);
        let mut out = std::io::stdout().lock();
        for i in 1..=CRASH_MOST {
            c.append(&[Entry { conversation: "c".into(), id: format!("m{i}"), seq: i, text: "acknowledged".into() }]).map_err(|e| anyhow::anyhow!("{e}"))?;
            writeln!(out, "acked m{i}")?;
            out.flush()?;
        }
        Ok(())
    }

    /// CP-T1.1 (seam review 2): every append the store acknowledged is there after the process dies with no close and
    /// more appends under way: a child of this test binary appends, saying each acknowledged, and is killed; the store
    /// opened again holds every one it said (read by the feed from the start), on each engine. CONVO-4 and the load
    /// scenario reopen only after a clean close, which a store that made writes durable at close alone would pass.
    #[test]
    fn cp_t1_1_every_acknowledged_append_stays_through_a_crash() -> anyhow::Result<()> {
        use std::io::BufRead;
        if let Ok(dir) = std::env::var(CRASH_DIR) {
            return crash_child(std::path::Path::new(&dir));
        }
        for kind in super::super::Kind::ALL {
            let dir = tempfile::tempdir()?;
            let mut child = std::process::Command::new(std::env::current_exe()?)
                .args(["--exact", "convo::seam::tests::cp_t1_1_every_acknowledged_append_stays_through_a_crash", "--nocapture", "--test-threads=1"])
                .env(CRASH_DIR, dir.path())
                .env(CRASH_ENGINE, kind.to_string())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .spawn()?;
            let out = child.stdout.take().ok_or_else(|| anyhow::anyhow!("no output from the child"))?;
            let mut out = std::io::BufReader::new(out);
            let mut acked = HashSet::new();
            let mut killed = false;
            let mut line = String::new();
            // Every whole line it said counts, those still in the pipe after the kill too; a line cut by the kill doesn't.
            loop {
                line.clear();
                if out.read_line(&mut line)? == 0 {
                    break;
                }
                if let Some(id) = line.strip_prefix("acked ").and_then(|l| l.strip_suffix('\n')) {
                    acked.insert(id.trim_end_matches('\r').to_owned());
                }
                if !killed && acked.len() >= CRASH_AFTER {
                    child.kill()?;
                    killed = true;
                }
            }
            let status = child.wait()?;
            anyhow::ensure!(killed, "{kind:?}: the child ended ({status}) after {} acknowledged appends, before it was killed", acked.len());
            let f = Fixture::new(kind, Some(dir));
            let c = kiln_seams::conversations::ConversationsFixture::open(&f).map_err(|e| anyhow::anyhow!("{kind:?}: opened again: {e}"))?;
            let top = c.top().map_err(|e| anyhow::anyhow!("{e}"))?;
            let mut seen = Vec::new();
            kiln_seams::conversations::follow(c.as_ref(), 0, top, 1 << 20, &mut seen).map_err(|e| anyhow::anyhow!("{kind:?}: {e}"))?;
            let kept: HashSet<String> = seen.into_iter().map(|ch| ch.entry.id).collect();
            let lost: Vec<&String> = acked.iter().filter(|id| !kept.contains(*id)).collect();
            anyhow::ensure!(lost.is_empty(), "{kind:?}: {} of {} acknowledged appends lost in the crash: {lost:?}", lost.len(), acked.len());
            c.close().map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        Ok(())
    }

    fn knob(name: &str, default: usize) -> anyhow::Result<usize> {
        match std::env::var(name) {
            Ok(v) => Ok(v.trim().parse()?),
            Err(_) => Ok(default),
        }
    }

    /// CP-T1.1: the seam's load scenario on each engine, Turso the control, up the concurrency ladder (seam review 11):
    /// at each rung as many writers as readers, many conversations written at once, readers following the feed waiting
    /// on each commit, a restart with appends and reads in flight; each reader's feed held to exactly once, in order,
    /// as it goes, and a line a rung and engine printed with what was measured, the store's memory apart from the
    /// harness's. Same driver, adapter and process for both engines. Its scale comes from KILN_CONVO_LOAD_* (a tiny one
    /// unset: rungs 1 and 8); the run of record sets KILN_CONVO_LOAD_LADDER=1,8,32,128,512,2048,4096 on Linux.
    ///
    /// Each engine runs each A/B's arm against the control (lane vendor-amphora-11): `opened=Set readers=One` (today's),
    /// `opened=Ask` (the store asked whether a session's row is there, each append) and `readers=Cores` (a reader a
    /// core), and `ask-cores` (both: an `ask` append's read waits behind the followers' on one reader, so this tells
    /// the cost of asking from the cost of the queue). A `convo-ab` line a rung and arm says what decides each: append
    /// p50/p99/p99.9 and the bytes held an open conversation (the set's own, counted, and the process's growth, Linux's)
    /// for `opened`; the feed's lag in revisions and its read time p50/p99/p99.9 for `readers`. KILN_CONVO_LOAD_ARMS
    /// picks arms by name (`control,ask,cores,ask-cores`, all of them unset).
    #[test]
    fn cp_t1_1_the_load_scenario_runs_on_each_engine() -> anyhow::Result<()> {
        let base = Load {
            conversations: knob("KILN_CONVO_LOAD_CONVERSATIONS", 16)?,
            writers: 0,
            appends: knob("KILN_CONVO_LOAD_APPENDS", 25)?,
            readers: 0,
            text_bytes: knob("KILN_CONVO_LOAD_TEXT_BYTES", 600)?,
            page_bytes: knob("KILN_CONVO_LOAD_PAGE_BYTES", 1 << 16)?,
            history_entries: knob("KILN_CONVO_LOAD_HISTORY_ENTRIES", 50)?,
            restart: knob("KILN_CONVO_LOAD_RESTART", 1)? != 0,
        };
        let rungs: Vec<usize> = match std::env::var("KILN_CONVO_LOAD_LADDER") {
            Ok(v) => v.split(',').map(|n| n.trim().parse::<usize>()).collect::<Result<_, _>>()?,
            Err(_) => kiln_seams::conversations::LADDER.iter().copied().take(2).collect(),
        };
        let all = [("control", Opened::Set, Readers::One), ("ask", Opened::Ask, Readers::One), ("cores", Opened::Set, Readers::Cores), ("ask-cores", Opened::Ask, Readers::Cores)];
        let arms: Vec<(&str, Opened, Readers)> = match std::env::var("KILN_CONVO_LOAD_ARMS") {
            Ok(v) => v.split(',').map(|name| all.iter().find(|(n, ..)| *n == name.trim()).copied().ok_or_else(|| anyhow::anyhow!("KILN_CONVO_LOAD_ARMS: no arm `{name}` (control, ask, cores, ask-cores)"))).collect::<anyhow::Result<_>>()?,
            Err(_) => all.to_vec(),
        };
        println!("convo-load-platform os={} arch={} cpus={} readers_cores={} tmp={}", std::env::consts::OS, std::env::consts::ARCH, std::thread::available_parallelism().map_or(0, |n| n.get()), Readers::Cores.count(), std::env::temp_dir().display());
        for kind in super::super::Kind::ALL {
            for &(arm, opened, readers) in &arms {
                // Each rung's fixture's count of the set's bytes, in rung order.
                let mut held = Vec::with_capacity(rungs.len());
                let reports = kiln_seams::conversations::ladder(&base, &rungs, || {
                    let f = Fixture::arm(kind, Some(tempfile::tempdir().map_err(|e| e.to_string())?), opened, readers);
                    held.push(Arc::clone(&f.held_most));
                    Ok(f)
                })
                .map_err(|e| anyhow::anyhow!("{kind:?} {arm}: {e}"))?;
                for ((n, r), held) in reports.into_iter().zip(held) {
                    anyhow::ensure!(r.appended == n.saturating_mul(base.appends), "{kind:?} {arm} at {n}: {r:?}");
                    let open = base.conversations.max(n).max(1);
                    let set_bytes = held.load(Ordering::Relaxed);
                    let (a, lag, feed) = (&r.append_us, &r.feed_lag_revs, &r.feed_us);
                    println!(
                        "convo-ab {kind:?} arm={arm} rung={n} append_us={}/{}/{} feed_lag_revs={}/{}/{} feed_us={}/{}/{} set_bytes_per_conversation={} store_bytes_per_conversation={} conversations={open}",
                        a.p50,
                        a.p99,
                        a.p999,
                        lag.p50,
                        lag.p99,
                        lag.p999,
                        feed.p50,
                        feed.p99,
                        feed.p999,
                        set_bytes / open,
                        r.store_bytes.map_or_else(|| "none".to_owned(), |b| (b / u64::try_from(open).unwrap_or(u64::MAX)).to_string()),
                    );
                    println!("convo-load {kind:?} arm={arm} rung={n} {r:?}");
                }
            }
        }
        Ok(())
    }
}
