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

    /// The store's error, or Closed when the handle was let go of meanwhile: what was under way as it closed (its writer
    /// gone with it) is refused Closed, as what's asked after is, and never written once refused.
    fn failed(&self, e: anyhow::Error) -> Error {
        if self.closed.load(Ordering::Acquire) { Error::Closed } else { failed(e) }
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
        // Sessions given their row by this append: theirs to give again if it isn't written.
        let mut new = Vec::new();
        {
            let mut opened = self.opened.lock();
            for e in entries {
                if opened.insert(e.conversation.clone()) {
                    new.push(e.conversation.clone());
                    out.push(Row::Session { session: e.conversation.clone(), info: None, meta: None, state: None, changes: None, epoch: None, open: true, at: now });
                }
                let event = Event { id: e.id.clone(), seq: e.seq, at: i64::try_from(e.seq).unwrap_or(i64::MAX), parent: None, by: None, body: Body::Assistant { text: e.text.clone(), streaming: false } };
                out.extend(rows::of(&e.conversation, &Update::Events { events: vec![event] }, now));
            }
        }
        // Ok only once the commit these rows went in was written: the writer's own outcome (CP-T1.1).
        self.store.put_committed(out).map_err(|e| {
            let mut opened = self.opened.lock();
            for s in &new {
                opened.remove(s);
            }
            self.failed(e)
        })
    }

    fn history(&self, conversation: &str, before: Option<u64>, limit: usize) -> Result<Vec<Entry>, Error> {
        self.live()?;
        Ok(self.store.events(conversation, before, limit).map_err(|e| self.failed(e))?.iter().map(entry_of).collect())
    }

    fn changes(&self, from: u64, max_bytes: usize) -> Result<Page, Error> {
        self.live()?;
        // The store's feed hands on its sessions' rows too: past a page of them alone, on to the next.
        let mut at = from;
        loop {
            let batch = self.store.changes(at, max_bytes).map_err(|e| self.failed(e))?;
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
        // store's writer sets the top it committed, then tells its watchers of each commit and of its close
        // (`Store::watch`, `Store::committed_top`): a reader woken asks nothing of the writer.
        let commits = self.store.watch();
        loop {
            self.live()?;
            let top = self.store.committed_top();
            if top > after {
                return Ok(top);
            }
            futures::executor::block_on(commits.notified());
        }
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
    use kiln_seams::conversations::{Load, cases};

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
            let f = Fixture { kind, dir: Some(dir) };
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
        println!("convo-load-platform os={} arch={} cpus={} tmp={}", std::env::consts::OS, std::env::consts::ARCH, std::thread::available_parallelism().map_or(0, |n| n.get()), std::env::temp_dir().display());
        for kind in super::super::Kind::ALL {
            let reports = kiln_seams::conversations::ladder(&base, &rungs, || Ok(Fixture { kind, dir: Some(tempfile::tempdir().map_err(|e| e.to_string())?) })).map_err(|e| anyhow::anyhow!("{kind:?}: {e}"))?;
            for (n, r) in reports {
                anyhow::ensure!(r.appended == n.saturating_mul(base.appends), "{kind:?} at {n}: {r:?}");
                println!("convo-load {kind:?} rung={n} {r:?}");
            }
        }
        Ok(())
    }
}
